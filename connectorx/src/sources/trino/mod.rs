use std::{marker::PhantomData, sync::Arc};

use chrono::{NaiveDate, NaiveDateTime, NaiveTime};
use fehler::{throw, throws};
use prusto::{auth::Auth, Client, ClientBuilder, DataSet, Presto, Row};
use serde_json::Value;
use sqlparser::dialect::GenericDialect;
use std::convert::TryFrom;
use tokio::runtime::Runtime;

use crate::{
    data_order::DataOrder,
    errors::ConnectorXError,
    sources::Produce,
    sql::{limit1_query, CXQuery},
};

pub use self::{errors::TrinoSourceError, typesystem::TrinoTypeSystem};
use urlencoding::decode;

use super::{PartitionParser, Source, SourcePartition};

use anyhow::anyhow;

pub mod errors;
pub mod typesystem;

#[throws(TrinoSourceError)]
fn get_total_rows(rt: Arc<Runtime>, client: Arc<Client>, query: &CXQuery<String>) -> usize {
    rt.block_on(client.get_all::<Row>(query.to_string()))
        .map_err(TrinoSourceError::PrustoError)?
        .len()
}

pub struct TrinoSource {
    client: Arc<Client>,
    rt: Arc<Runtime>,
    origin_query: Option<String>,
    queries: Vec<CXQuery<String>>,
    names: Vec<String>,
    schema: Vec<TrinoTypeSystem>,
}

impl TrinoSource {
    #[throws(TrinoSourceError)]
    pub fn new(rt: Arc<Runtime>, conn: &str) -> Self {
        let decoded_conn = decode(conn)?.into_owned();

        let url = decoded_conn
            .parse::<url::Url>()
            .map_err(TrinoSourceError::UrlParseError)?;

        let client = ClientBuilder::new(url.username(), url.host().unwrap().to_owned())
            .port(url.port().unwrap_or(8080))
            .auth(Auth::Basic(
                url.username().to_owned(),
                url.password().map(|x| x.to_owned()),
            ))
            .ssl(prusto::ssl::Ssl { root_cert: None })
            .secure(url.scheme() == "trino+https")
            .catalog(url.path_segments().unwrap().last().unwrap_or("hive"))
            .build()
            .map_err(TrinoSourceError::PrustoError)?;

        Self {
            client: Arc::new(client),
            rt,
            origin_query: None,
            queries: vec![],
            names: vec![],
            schema: vec![],
        }
    }
}

impl Source for TrinoSource
where
    TrinoSourcePartition: SourcePartition<TypeSystem = TrinoTypeSystem, Error = TrinoSourceError>,
{
    const DATA_ORDERS: &'static [DataOrder] = &[DataOrder::RowMajor];
    type TypeSystem = TrinoTypeSystem;
    type Partition = TrinoSourcePartition;
    type Error = TrinoSourceError;

    #[throws(TrinoSourceError)]
    fn set_data_order(&mut self, data_order: DataOrder) {
        if !matches!(data_order, DataOrder::RowMajor) {
            throw!(ConnectorXError::UnsupportedDataOrder(data_order));
        }
    }

    fn set_queries<Q: ToString>(&mut self, queries: &[CXQuery<Q>]) {
        self.queries = queries.iter().map(|q| q.map(Q::to_string)).collect();
    }

    fn set_origin_query(&mut self, query: Option<String>) {
        self.origin_query = query;
    }

    #[throws(TrinoSourceError)]
    fn fetch_metadata(&mut self) {
        assert!(!self.queries.is_empty());

        // TODO: prevent from running the same query multiple times (limit1 + no limit)
        let first_query = &self.queries[0];
        let cxq = limit1_query(first_query, &GenericDialect {})?;

        let dataset: DataSet<Row> = self
            .rt
            .block_on(self.client.get_all::<Row>(cxq.to_string()))
            .map_err(TrinoSourceError::PrustoError)?;

        let schema = dataset.split().0;

        for (name, t) in schema {
            self.names.push(name.clone());
            self.schema.push(TrinoTypeSystem::try_from(t.clone())?);
        }
    }

    #[throws(TrinoSourceError)]
    fn result_rows(&mut self) -> Option<usize> {
        match &self.origin_query {
            Some(q) => {
                let cxq = CXQuery::Naked(q.clone());
                let nrows = get_total_rows(self.rt.clone(), self.client.clone(), &cxq)?;
                Some(nrows)
            }
            None => None,
        }
    }

    fn names(&self) -> Vec<String> {
        self.names.clone()
    }

    fn schema(&self) -> Vec<Self::TypeSystem> {
        self.schema.clone()
    }

    #[throws(TrinoSourceError)]
    fn partition(self) -> Vec<Self::Partition> {
        let mut ret = vec![];

        for query in self.queries {
            ret.push(TrinoSourcePartition::new(
                self.client.clone(),
                query,
                self.schema.clone(),
                self.rt.clone(),
            )?);
        }
        ret
    }
}

pub struct TrinoSourcePartition {
    client: Arc<Client>,
    query: CXQuery<String>,
    schema: Vec<TrinoTypeSystem>,
    rt: Arc<Runtime>,
    nrows: usize,
}

impl TrinoSourcePartition {
    #[throws(TrinoSourceError)]
    pub fn new(
        client: Arc<Client>,
        query: CXQuery<String>,
        schema: Vec<TrinoTypeSystem>,
        rt: Arc<Runtime>,
    ) -> Self {
        Self {
            client,
            query: query.clone(),
            schema: schema.to_vec(),
            rt,
            nrows: 0,
        }
    }
}

impl SourcePartition for TrinoSourcePartition {
    type TypeSystem = TrinoTypeSystem;
    type Parser<'a> = TrinoSourcePartitionParser<'a>;
    type Error = TrinoSourceError;

    #[throws(TrinoSourceError)]
    fn result_rows(&mut self) {
        self.nrows = get_total_rows(self.rt.clone(), self.client.clone(), &self.query)?;
    }

    #[throws(TrinoSourceError)]
    fn parser(&mut self) -> Self::Parser<'_> {
        TrinoSourcePartitionParser::new(
            self.rt.clone(),
            self.client.clone(),
            self.query.clone(),
            &self.schema,
        )?
    }

    fn nrows(&self) -> usize {
        self.nrows
    }

    fn ncols(&self) -> usize {
        self.schema.len()
    }
}

pub struct TrinoSourcePartitionParser<'a> {
    rows: Vec<Row>,
    ncols: usize,
    current_col: usize,
    current_row: usize,
    _phantom: &'a PhantomData<DataSet<Row>>,
}

impl<'a> TrinoSourcePartitionParser<'a> {
    #[throws(TrinoSourceError)]
    pub fn new(
        rt: Arc<Runtime>,
        client: Arc<Client>,
        query: CXQuery,
        schema: &[TrinoTypeSystem],
    ) -> Self {
        let rows = client.get_all::<Row>(query.to_string());
        let data = rt.block_on(rows).map_err(TrinoSourceError::PrustoError)?;
        let rows = data.clone().into_vec();

        Self {
            rows,
            ncols: schema.len(),
            current_row: 0,
            current_col: 0,
            _phantom: &PhantomData,
        }
    }

    #[throws(TrinoSourceError)]
    fn next_loc(&mut self) -> (usize, usize) {
        let ret = (self.current_row, self.current_col);
        self.current_row += (self.current_col + 1) / self.ncols;
        self.current_col = (self.current_col + 1) % self.ncols;
        ret
    }
}

impl<'a> PartitionParser<'a> for TrinoSourcePartitionParser<'a> {
    type TypeSystem = TrinoTypeSystem;
    type Error = TrinoSourceError;

    #[throws(TrinoSourceError)]
    fn fetch_next(&mut self) -> (usize, bool) {
        assert!(self.current_col == 0);

        // results are always fetched in a single batch for Prusto
        (self.rows.len(), true)
    }
}

macro_rules! impl_produce_int {
    ($($t: ty,)+) => {
        $(
            impl<'r, 'a> Produce<'r, $t> for TrinoSourcePartitionParser<'a> {
                type Error = TrinoSourceError;

                #[throws(TrinoSourceError)]
                fn produce(&'r mut self) -> $t {
                    let (ridx, cidx) = self.next_loc()?;
                    let value = &self.rows[ridx].value()[cidx];

                    match value {
                        Value::Number(x) => {
                            if (x.is_i64()) {
                                <$t>::try_from(x.as_i64().unwrap()).map_err(|_| anyhow!("Trino cannot parse Number at position: ({}, {})", ridx, cidx))?
                            } else {
                                throw!(anyhow!("Trino cannot parse Number at position: ({}, {})", ridx, cidx))
                            }
                        }
                        _ => throw!(anyhow!("Trino cannot parse Number at position: ({}, {})", ridx, cidx))
                    }
                }
            }

            impl<'r, 'a> Produce<'r, Option<$t>> for TrinoSourcePartitionParser<'a> {
                type Error = TrinoSourceError;

                #[throws(TrinoSourceError)]
                fn produce(&'r mut self) -> Option<$t> {
                    let (ridx, cidx) = self.next_loc()?;
                    let value = &self.rows[ridx].value()[cidx];

                    match value {
                        Value::Null => None,
                        Value::Number(x) => {
                            if (x.is_i64()) {
                                Some(<$t>::try_from(x.as_i64().unwrap()).map_err(|_| anyhow!("Trino cannot parse Number at position: ({}, {})", ridx, cidx))?)
                            } else {
                                throw!(anyhow!("Trino cannot parse Number at position: ({}, {})", ridx, cidx))
                            }
                        }
                        _ => throw!(anyhow!("Trino cannot parse Number at position: ({}, {})", ridx, cidx))
                    }
                }
            }
        )+
    };
}

macro_rules! impl_produce_float {
    ($($t: ty,)+) => {
        $(
            impl<'r, 'a> Produce<'r, $t> for TrinoSourcePartitionParser<'a> {
                type Error = TrinoSourceError;

                #[throws(TrinoSourceError)]
                fn produce(&'r mut self) -> $t {
                    let (ridx, cidx) = self.next_loc()?;
                    let value = &self.rows[ridx].value()[cidx];

                    match value {
                        Value::Number(x) => {
                            if (x.is_f64()) {
                                x.as_f64().unwrap() as $t
                            } else {
                                throw!(anyhow!("Trino cannot parse Number at position: ({}, {})", ridx, cidx))
                            }
                        }
                        _ => throw!(anyhow!("Trino cannot parse Number at position: ({}, {})", ridx, cidx))
                    }
                }
            }

            impl<'r, 'a> Produce<'r, Option<$t>> for TrinoSourcePartitionParser<'a> {
                type Error = TrinoSourceError;

                #[throws(TrinoSourceError)]
                fn produce(&'r mut self) -> Option<$t> {
                    let (ridx, cidx) = self.next_loc()?;
                    let value = &self.rows[ridx].value()[cidx];

                    match value {
                        Value::Null => None,
                        Value::Number(x) => {
                            if (x.is_f64()) {
                                Some(x.as_f64().unwrap() as $t)
                            } else {
                                throw!(anyhow!("Trino cannot parse Number at position: ({}, {})", ridx, cidx))
                            }
                        }
                        _ => throw!(anyhow!("Trino cannot parse Number at position: ({}, {})", ridx, cidx))
                    }
                }
            }
        )+
    };
}

macro_rules! impl_produce_text {
    ($($t: ty,)+) => {
        $(
            impl<'r, 'a> Produce<'r, $t> for TrinoSourcePartitionParser<'a> {
                type Error = TrinoSourceError;

                #[throws(TrinoSourceError)]
                fn produce(&'r mut self) -> $t {
                    let (ridx, cidx) = self.next_loc()?;
                    let value = &self.rows[ridx].value()[cidx];

                    match value {
                        Value::String(x) => {
                            x.parse().map_err(|_| anyhow!("Trino cannot parse String at position: ({}, {}): {:?}", ridx, cidx, value))?
                        }
                        _ => throw!(anyhow!("Trino unknown value at position: ({}, {}): {:?}", ridx, cidx, value))
                    }
                }
            }

            impl<'r, 'a> Produce<'r, Option<$t>> for TrinoSourcePartitionParser<'a> {
                type Error = TrinoSourceError;

                #[throws(TrinoSourceError)]
                fn produce(&'r mut self) -> Option<$t> {
                    let (ridx, cidx) = self.next_loc()?;
                    let value = &self.rows[ridx].value()[cidx];

                    match value {
                        Value::Null => None,
                        Value::String(x) => {
                            Some(x.parse().map_err(|_| anyhow!("Trino cannot parse String at position: ({}, {}): {:?}", ridx, cidx, value))?)
                        }
                        _ => throw!(anyhow!("Trino unknown value at position: ({}, {}): {:?}", ridx, cidx, value))
                    }
                }
            }
        )+
    };
}

macro_rules! impl_produce_timestamp {
    ($($t: ty,)+) => {
        $(
            impl<'r, 'a> Produce<'r, $t> for TrinoSourcePartitionParser<'a> {
                type Error = TrinoSourceError;

                #[throws(TrinoSourceError)]
                fn produce(&'r mut self) -> $t {
                    let (ridx, cidx) = self.next_loc()?;
                    let value = &self.rows[ridx].value()[cidx];

                    match value {
                        Value::String(x) => NaiveDateTime::parse_from_str(x, "%Y-%m-%d %H:%M:%S%.f").map_err(|_| anyhow!("Trino cannot parse String at position: ({}, {}): {:?}", ridx, cidx, value))?,
                        _ => throw!(anyhow!("Trino unknown value at position: ({}, {}): {:?}", ridx, cidx, value))
                    }
                }
            }

            impl<'r, 'a> Produce<'r, Option<$t>> for TrinoSourcePartitionParser<'a> {
                type Error = TrinoSourceError;

                #[throws(TrinoSourceError)]
                fn produce(&'r mut self) -> Option<$t> {
                    let (ridx, cidx) = self.next_loc()?;
                    let value = &self.rows[ridx].value()[cidx];

                    match value {
                        Value::Null => None,
                        Value::String(x) => Some(NaiveDateTime::parse_from_str(x, "%Y-%m-%d %H:%M:%S%.f").map_err(|_| anyhow!("Trino cannot parse String at position: ({}, {}): {:?}", ridx, cidx, value))?),
                        _ => throw!(anyhow!("Trino unknown value at position: ({}, {}): {:?}", ridx, cidx, value))
                    }
                }
            }
        )+
    };
}

macro_rules! impl_produce_bool {
    ($($t: ty,)+) => {
        $(
            impl<'r, 'a> Produce<'r, $t> for TrinoSourcePartitionParser<'a> {
                type Error = TrinoSourceError;

                #[throws(TrinoSourceError)]
                fn produce(&'r mut self) -> $t {
                    let (ridx, cidx) = self.next_loc()?;
                    let value = &self.rows[ridx].value()[cidx];

                    match value {
                        Value::Bool(x) => *x,
                        _ => throw!(anyhow!("Trino unknown value at position: ({}, {}): {:?}", ridx, cidx, value))
                    }
                }
            }

            impl<'r, 'a> Produce<'r, Option<$t>> for TrinoSourcePartitionParser<'a> {
                type Error = TrinoSourceError;

                #[throws(TrinoSourceError)]
                fn produce(&'r mut self) -> Option<$t> {
                    let (ridx, cidx) = self.next_loc()?;
                    let value = &self.rows[ridx].value()[cidx];

                    match value {
                        Value::Null => None,
                        Value::Bool(x) => Some(*x),
                        _ => throw!(anyhow!("Trino unknown value at position: ({}, {}): {:?}", ridx, cidx, value))
                    }
                }
            }
        )+
    };
}

impl_produce_bool!(bool,);
impl_produce_int!(i8, i16, i32, i64,);
impl_produce_float!(f32, f64,);
impl_produce_timestamp!(NaiveDateTime,);
impl_produce_text!(String, char,);

impl<'r, 'a> Produce<'r, NaiveTime> for TrinoSourcePartitionParser<'a> {
    type Error = TrinoSourceError;

    #[throws(TrinoSourceError)]
    fn produce(&'r mut self) -> NaiveTime {
        let (ridx, cidx) = self.next_loc()?;
        let value = &self.rows[ridx].value()[cidx];

        match value {
            Value::String(x) => NaiveTime::parse_from_str(x, "%H:%M:%S%.f").map_err(|_| {
                anyhow!(
                    "Trino cannot parse String at position: ({}, {}): {:?}",
                    ridx,
                    cidx,
                    value
                )
            })?,
            _ => throw!(anyhow!(
                "Trino unknown value at position: ({}, {}): {:?}",
                ridx,
                cidx,
                value
            )),
        }
    }
}

impl<'r, 'a> Produce<'r, Option<NaiveTime>> for TrinoSourcePartitionParser<'a> {
    type Error = TrinoSourceError;

    #[throws(TrinoSourceError)]
    fn produce(&'r mut self) -> Option<NaiveTime> {
        let (ridx, cidx) = self.next_loc()?;
        let value = &self.rows[ridx].value()[cidx];

        match value {
            Value::Null => None,
            Value::String(x) => {
                Some(NaiveTime::parse_from_str(x, "%H:%M:%S%.f").map_err(|_| {
                    anyhow!(
                        "Trino cannot parse Time at position: ({}, {}): {:?}",
                        ridx,
                        cidx,
                        value
                    )
                })?)
            }
            _ => throw!(anyhow!(
                "Trino unknown value at position: ({}, {}): {:?}",
                ridx,
                cidx,
                value
            )),
        }
    }
}

impl<'r, 'a> Produce<'r, NaiveDate> for TrinoSourcePartitionParser<'a> {
    type Error = TrinoSourceError;

    #[throws(TrinoSourceError)]
    fn produce(&'r mut self) -> NaiveDate {
        let (ridx, cidx) = self.next_loc()?;
        let value = &self.rows[ridx].value()[cidx];

        match value {
            Value::String(x) => NaiveDate::parse_from_str(x, "%Y-%m-%d").map_err(|_| {
                anyhow!(
                    "Trino cannot parse Date at position: ({}, {}): {:?}",
                    ridx,
                    cidx,
                    value
                )
            })?,
            _ => throw!(anyhow!(
                "Trino unknown value at position: ({}, {}): {:?}",
                ridx,
                cidx,
                value
            )),
        }
    }
}

impl<'r, 'a> Produce<'r, Option<NaiveDate>> for TrinoSourcePartitionParser<'a> {
    type Error = TrinoSourceError;

    #[throws(TrinoSourceError)]
    fn produce(&'r mut self) -> Option<NaiveDate> {
        let (ridx, cidx) = self.next_loc()?;
        let value = &self.rows[ridx].value()[cidx];

        match value {
            Value::Null => None,
            Value::String(x) => Some(NaiveDate::parse_from_str(x, "%Y-%m-%d").map_err(|_| {
                anyhow!(
                    "Trino cannot parse Date at position: ({}, {}): {:?}",
                    ridx,
                    cidx,
                    value
                )
            })?),
            _ => throw!(anyhow!(
                "Trino unknown value at position: ({}, {}): {:?}",
                ridx,
                cidx,
                value
            )),
        }
    }
}
