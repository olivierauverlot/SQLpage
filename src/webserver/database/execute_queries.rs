use anyhow::anyhow;
use futures_util::stream::Stream;
use futures_util::StreamExt;
use std::borrow::Cow;
use std::collections::HashMap;

use super::csv_import::run_csv_import;
use super::sql::{ParsedSqlFile, ParsedStatement, StmtWithParams};
use crate::webserver::database::sql_pseudofunctions::extract_req_param;
use crate::webserver::database::sql_to_json::row_to_string;
use crate::webserver::http::SingleOrVec;
use crate::webserver::http_request_info::RequestInfo;

use sqlx::any::{AnyArguments, AnyQueryResult, AnyRow, AnyStatement, AnyTypeInfo};
use sqlx::pool::PoolConnection;
use sqlx::{Any, AnyConnection, Arguments, Either, Executor, Statement};

use super::sql_pseudofunctions::StmtParam;
use super::{highlight_sql_error, Database, DbItem};

impl Database {
    pub(crate) async fn prepare_with(
        &self,
        query: &str,
        param_types: &[AnyTypeInfo],
    ) -> anyhow::Result<AnyStatement<'static>> {
        self.connection
            .prepare_with(query, param_types)
            .await
            .map(|s| s.to_owned())
            .map_err(|e| highlight_sql_error("Failed to prepare SQL statement", query, e))
    }
}

pub fn stream_query_results<'a>(
    db: &'a Database,
    sql_file: &'a ParsedSqlFile,
    request: &'a mut RequestInfo,
) -> impl Stream<Item = DbItem> + 'a {
    async_stream::try_stream! {
        let mut connection_opt = None;
        for res in &sql_file.statements {
            match res {
                ParsedStatement::CsvImport(csv_import) => {
                    let connection = take_connection(db, &mut connection_opt).await?;
                    log::debug!("Executing CSV import: {:?}", csv_import);
                    run_csv_import(connection, csv_import, request).await?;
                },
                ParsedStatement::StmtWithParams(stmt) => {
                    let query = bind_parameters(stmt, request).await?;
                    let connection = take_connection(db, &mut connection_opt).await?;
                    log::trace!("Executing query {:?}", query.sql);
                    let mut stream = connection.fetch_many(query);
                    while let Some(elem) = stream.next().await {
                        let is_err = elem.is_err();
                        yield parse_single_sql_result(&stmt.query, elem);
                        if is_err {
                            break;
                        }
                    }
                },
                ParsedStatement::SetVariable { variable, value} => {
                    let query = bind_parameters(value, request).await?;
                    let connection = take_connection(db, &mut connection_opt).await?;
                    log::debug!("Executing query to set the {variable:?} variable: {:?}", query.sql);
                    let value: Option<String> = connection.fetch_optional(query).await?.as_ref().and_then(row_to_string);
                    let (vars, name) = vars_and_name(request, variable)?;
                    if let Some(value) = value {
                        log::debug!("Setting variable {name} to {value:?}");
                        vars.insert(name.clone(), SingleOrVec::Single(value));
                    } else {
                        log::debug!("Removing variable {name}");
                        vars.remove(&name);
                    }
                },
                ParsedStatement::StaticSimpleSelect(value) => {
                    yield DbItem::Row(value.clone().into())
                }
                ParsedStatement::Error(e) => yield DbItem::Error(clone_anyhow_err(e)),
            }
        }
    }
    .map(|res| res.unwrap_or_else(DbItem::Error))
}

fn vars_and_name<'a>(
    request: &'a mut RequestInfo,
    variable: &StmtParam,
) -> anyhow::Result<(&'a mut HashMap<String, SingleOrVec>, String)> {
    match variable {
        StmtParam::Get(name) | StmtParam::GetOrPost(name) => {
            let vars = &mut request.get_variables;
            Ok((vars, name.clone()))
        }
        StmtParam::Post(name) => {
            let vars = &mut request.post_variables;
            Ok((vars, name.clone()))
        }
        _ => Err(anyhow!(
            "Only GET and POST variables can be set, not {variable:?}"
        )),
    }
}

async fn take_connection<'a, 'b>(
    db: &'a Database,
    conn: &'b mut Option<PoolConnection<sqlx::Any>>,
) -> anyhow::Result<&'b mut AnyConnection> {
    match conn {
        Some(c) => Ok(c),
        None => match db.connection.acquire().await {
            Ok(c) => {
                log::debug!("Acquired a database connection");
                *conn = Some(c);
                Ok(conn.as_mut().unwrap())
            }
            Err(e) => {
                let err_msg = format!("Unable to acquire a database connection to execute the SQL file. All of the {} {:?} connections are busy.", db.connection.size(), db.connection.any_kind());
                Err(anyhow::Error::new(e).context(err_msg))
            }
        },
    }
}

#[inline]
fn parse_single_sql_result(sql: &str, res: sqlx::Result<Either<AnyQueryResult, AnyRow>>) -> DbItem {
    match res {
        Ok(Either::Right(r)) => DbItem::Row(super::sql_to_json::row_to_json(&r)),
        Ok(Either::Left(res)) => {
            log::debug!("Finished query with result: {:?}", res);
            DbItem::FinishedQuery
        }
        Err(err) => DbItem::Error(highlight_sql_error(
            "Failed to execute SQL statement",
            sql,
            err,
        )),
    }
}

fn clone_anyhow_err(err: &anyhow::Error) -> anyhow::Error {
    let mut e = anyhow!("SQLPage could not parse and prepare this SQL statement");
    for c in err.chain().rev() {
        e = e.context(c.to_string());
    }
    e
}

async fn bind_parameters<'a>(
    stmt: &'a StmtWithParams,
    request: &'a RequestInfo,
) -> anyhow::Result<StatementWithParams<'a>> {
    let sql = stmt.query.as_str();
    log::debug!("Preparing statement: {}", sql);
    let mut arguments = AnyArguments::default();
    for (param_idx, param) in stmt.params.iter().enumerate() {
        log::trace!("\tevaluating parameter {}: {:?}", param_idx + 1, param);
        let argument = extract_req_param(param, request).await?;
        log::debug!(
            "\tparameter {}: {}",
            param_idx + 1,
            argument.as_ref().unwrap_or(&Cow::Borrowed("NULL"))
        );
        match argument {
            None => arguments.add(None::<String>),
            Some(Cow::Owned(s)) => arguments.add(s),
            Some(Cow::Borrowed(v)) => arguments.add(v),
        }
    }
    Ok(StatementWithParams { sql, arguments })
}

pub struct StatementWithParams<'a> {
    sql: &'a str,
    arguments: AnyArguments<'a>,
}

impl<'q> sqlx::Execute<'q, Any> for StatementWithParams<'q> {
    fn sql(&self) -> &'q str {
        self.sql
    }

    fn statement(&self) -> Option<&<Any as sqlx::database::HasStatement<'q>>::Statement> {
        None
    }

    fn take_arguments(&mut self) -> Option<<Any as sqlx::database::HasArguments<'q>>::Arguments> {
        Some(std::mem::take(&mut self.arguments))
    }

    fn persistent(&self) -> bool {
        // Let sqlx create a prepared statement the first time it is executed, and then reuse it.
        true
    }
}
