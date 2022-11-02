use chrono::DateTime;
use clickhouse_rs::{types, types::Block, types::SqlType, ClientHandle, Pool};
use pgx::log::PgSqlErrorCode;
use pgx::prelude::Timestamp;
use std::collections::HashMap;
use time::OffsetDateTime;

use supabase_wrappers::{
    create_async_runtime, report_error, require_option, Cell, ForeignDataWrapper, Limit, Qual, Row,
    Runtime, Sort,
};

pub(crate) struct ClickHouseFdw {
    rt: Runtime,
    client: Option<ClientHandle>,
    table: String,
    rowid_col: String,
    scan_blk: Option<Block<types::Complex>>,
    row_idx: usize,
}

impl ClickHouseFdw {
    pub fn new(options: &HashMap<String, String>) -> Self {
        let rt = create_async_runtime();
        let conn_str = require_option("conn_string", options);
        let client = if conn_str.is_empty() {
            None
        } else {
            let pool = Pool::new(conn_str.as_str());
            rt.block_on(pool.get_handle()).map_or_else(
                |err| {
                    report_error(
                        PgSqlErrorCode::ERRCODE_FDW_UNABLE_TO_ESTABLISH_CONNECTION,
                        &format!("connection failed: {}", err),
                    );
                    None
                },
                |client| Some(client),
            )
        };
        ClickHouseFdw {
            rt,
            client,
            table: "".to_string(),
            rowid_col: "".to_string(),
            scan_blk: None,
            row_idx: 0,
        }
    }

    fn deparse(&self, quals: &Vec<Qual>, columns: &Vec<String>) -> String {
        let tgts = columns.join(", ");
        let sql = if quals.is_empty() {
            format!("select {} from {}", tgts, &self.table)
        } else {
            let cond = quals
                .iter()
                .map(|q| q.deparse())
                .collect::<Vec<String>>()
                .join(" and ");
            format!("select {} from {} where {}", tgts, &self.table, cond)
        };
        sql
    }
}

impl ForeignDataWrapper for ClickHouseFdw {
    fn get_rel_size(
        &mut self,
        quals: &Vec<Qual>,
        columns: &Vec<String>,
        _sorts: &Vec<Sort>,
        _limit: &Option<Limit>,
        options: &HashMap<String, String>,
    ) -> (i64, i32) {
        self.table = require_option("table", options);
        self.rowid_col = require_option("rowid_column", options);
        if self.table.is_empty() || self.rowid_col.is_empty() {
            return (0, 0);
        }

        let sql = self.deparse(quals, columns);

        if let Some(ref mut client) = self.client {
            // for simplicity purpose, we fetch whole query result to local,
            // may need optimization in the future.
            match self.rt.block_on(client.query(&sql).fetch_all()) {
                Ok(block) => {
                    let rows = block.row_count();
                    let width = block.column_count() * 8;
                    self.scan_blk = Some(block);
                    return (rows as i64, width as i32);
                }
                Err(err) => report_error(
                    PgSqlErrorCode::ERRCODE_FDW_ERROR,
                    &format!("query failed: {}", err),
                ),
            }
        }
        (0, 0)
    }

    fn begin_scan(
        &mut self,
        _quals: &Vec<Qual>,
        _columns: &Vec<String>,
        _sorts: &Vec<Sort>,
        _limit: &Option<Limit>,
        _options: &HashMap<String, String>,
    ) {
        self.row_idx = 0;
    }

    fn iter_scan(&mut self) -> Option<Row> {
        if let Some(block) = &self.scan_blk {
            let mut ret = Row::new();
            let mut rows = block.rows();

            if let Some(row) = rows.nth(self.row_idx) {
                for i in 0..block.column_count() {
                    let col_name = row.name(i).unwrap();
                    let sql_type = row.sql_type(i).unwrap();
                    let cell = match sql_type {
                        SqlType::UInt8 => {
                            // Bool is stored as UInt8 in ClickHouse, so we treat it as bool here
                            let value = row.get::<u8, usize>(i).unwrap();
                            Cell::Bool(value != 0)
                        }
                        SqlType::Int16 => {
                            let value = row.get::<i16, usize>(i).unwrap();
                            Cell::I16(value)
                        }
                        SqlType::Int32 => {
                            let value = row.get::<i32, usize>(i).unwrap();
                            Cell::I32(value)
                        }
                        SqlType::UInt32 => {
                            let value = row.get::<u32, usize>(i).unwrap();
                            Cell::I64(value as i64)
                        }
                        SqlType::Float32 => {
                            let value = row.get::<f32, usize>(i).unwrap();
                            Cell::F32(value)
                        }
                        SqlType::Float64 => {
                            let value = row.get::<f64, usize>(i).unwrap();
                            Cell::F64(value)
                        }
                        SqlType::UInt64 => {
                            let value = row.get::<u64, usize>(i).unwrap();
                            Cell::I64(value as i64)
                        }
                        SqlType::Int64 => {
                            let value = row.get::<i64, usize>(i).unwrap();
                            Cell::I64(value)
                        }
                        SqlType::String => {
                            let value = row.get::<String, usize>(i).unwrap();
                            Cell::String(value)
                        }
                        SqlType::DateTime(_) => {
                            let value = row.get::<DateTime<_>, usize>(i).unwrap();
                            let dt = OffsetDateTime::from_unix_timestamp_nanos(
                                (value.timestamp_nanos()) as i128,
                            )
                            .unwrap();
                            let ts = Timestamp::try_from(dt).unwrap();
                            Cell::Timestamp(ts)
                        }
                        _ => {
                            report_error(
                                PgSqlErrorCode::ERRCODE_FDW_INVALID_DATA_TYPE,
                                &format!("data type {} is not supported", sql_type.to_string()),
                            );
                            return None;
                        }
                    };
                    ret.push(col_name, Some(cell));
                }

                self.row_idx += 1;
                return Some(ret);
            }
        }
        None
    }

    fn end_scan(&mut self) {
        self.scan_blk.take();
    }

    fn begin_modify(&mut self, options: &HashMap<String, String>) {
        self.table = require_option("table", options);
        self.rowid_col = require_option("rowid_column", options);
    }

    fn insert(&mut self, src: &Row) {
        if let Some(ref mut client) = self.client {
            let mut row = Vec::new();
            for (col_name, cell) in src.iter() {
                let col_name = col_name.to_owned();
                if let Some(cell) = cell {
                    match cell {
                        Cell::Bool(v) => row.push((col_name, types::Value::from(*v))),
                        Cell::F64(v) => row.push((col_name, types::Value::from(*v))),
                        Cell::I64(v) => row.push((col_name, types::Value::from(*v))),
                        Cell::String(v) => row.push((col_name, types::Value::from(v.as_str()))),
                        _ => report_error(
                            PgSqlErrorCode::ERRCODE_FDW_INVALID_DATA_TYPE,
                            &format!("field type {:?} not supported", cell),
                        ),
                    }
                }
            }
            let mut block = Block::new();
            block.push(row).unwrap();

            // execute query on ClickHouse
            if let Err(err) = self.rt.block_on(client.insert(&self.table, block)) {
                report_error(
                    PgSqlErrorCode::ERRCODE_FDW_ERROR,
                    &format!("insert failed: {}", err),
                );
            }
        }
    }

    fn update(&mut self, rowid: &Cell, new_row: &Row) {
        if let Some(ref mut client) = self.client {
            let mut sets = Vec::new();
            for (col, cell) in new_row.iter() {
                if col == &self.rowid_col {
                    continue;
                }
                if let Some(cell) = cell {
                    sets.push(format!("{} = {}", col, cell));
                } else {
                    sets.push(format!("{} = null", col));
                }
            }
            let sql = format!(
                "alter table {} update {} where {} = {}",
                self.table,
                sets.join(", "),
                self.rowid_col,
                rowid
            );

            // execute query on ClickHouse
            if let Err(err) = self.rt.block_on(client.execute(&sql)) {
                report_error(
                    PgSqlErrorCode::ERRCODE_FDW_ERROR,
                    &format!("update failed: {}", err),
                );
            }
        }
    }

    fn end_modify(&mut self) {}

    fn delete(&mut self, rowid: &Cell) {
        if let Some(ref mut client) = self.client {
            let sql = format!(
                "alter table {} delete where {} = {}",
                self.table, self.rowid_col, rowid
            );

            // execute query on ClickHouse
            if let Err(err) = self.rt.block_on(client.execute(&sql)) {
                report_error(
                    PgSqlErrorCode::ERRCODE_FDW_ERROR,
                    &format!("delete failed: {}", err),
                );
            }
        }
    }
}