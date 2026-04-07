//! MySQL query tool driven by runtime configuration loaded from `tools.yaml`.

use super::*;
use mysql::prelude::Queryable;
use mysql::{AccessMode, OptsBuilder, Pool, Row, TxOpts, Value as MySqlValue};
use serde::Deserialize;
use serde_json::{Map, Number};

pub struct MySqlTool;

#[derive(Debug, Deserialize)]
struct Input {
    query: String,
    max_rows: Option<usize>,
}

#[derive(Debug)]
struct QuerySetOutput {
    columns: Vec<String>,
    rows: Vec<serde_json::Value>,
    affected_rows: u64,
    last_insert_id: Option<u64>,
}

#[async_trait]
impl Tool for MySqlTool {
    fn name(&self) -> &str {
        "MySql"
    }

    fn description(&self) -> &str {
        "Execute a SQL query against the MySQL database configured in tools.yaml."
    }

    fn permission_level(&self) -> PermissionLevel {
        match global_tools_config().mysql {
            Some(cfg) if cfg.readonly => PermissionLevel::ReadOnly,
            Some(_) => PermissionLevel::Dangerous,
            None => PermissionLevel::Dangerous,
        }
    }
    fn category(&self) -> ToolCategory {
        ToolCategory::Custom
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "SQL query to execute against the configured MySQL database"
                },
                "max_rows": {
                    "type": "integer",
                    "description": "Maximum number of rows to return across all result sets (default 100, max 1000)"
                }
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let input: Input = match serde_json::from_value(input) {
            Ok(i) => i,
            Err(e) => return ToolResult::error(format!("Invalid input: {e}")),
        };

        let tools_config = match ctx.extensions.get::<ToolsConfig>() {
            Some(cfg) => cfg,
            None => {
                return ToolResult::error(
                    "MySQL is not configured. Add a tools.yaml file with a mysql section.",
                )
            }
        };
        let config = match &tools_config.mysql {
            Some(cfg) => cfg.clone(),
            None => return ToolResult::error("MySQL is not configured in tools.yaml."),
        };

        if config.readonly {
            if let Err(e) = validate_readonly_query(&input.query) {
                return ToolResult::error(e);
            }
        }

        let max_rows = input.max_rows.unwrap_or(100).clamp(1, 1000);
        let query = input.query;

        let result =
            tokio::task::spawn_blocking(move || execute_mysql_query(&config, &query, max_rows))
                .await;

        match result {
            Ok(Ok((sets, truncated))) => {
                let content = format_result_sets(&sets, truncated, max_rows);
                let metadata = serde_json::json!({
                    "result_sets": sets.len(),
                    "truncated": truncated,
                });
                ToolResult::success(content).with_metadata(metadata)
            }
            Ok(Err(e)) => ToolResult::error(e),
            Err(e) => ToolResult::error(format!("MySQL task failed: {e}")),
        }
    }
}

fn execute_mysql_query(
    config: &MySqlToolConfig,
    query: &str,
    max_rows: usize,
) -> std::result::Result<(Vec<QuerySetOutput>, bool), String> {
    let mut builder = OptsBuilder::new()
        .ip_or_hostname(Some(config.host.clone()))
        .tcp_port(config.port)
        .user(Some(config.user.clone()))
        .pass(Some(config.password.clone()))
        .prefer_socket(false)
        .stmt_cache_size(Some(0));

    if let Some(database) = &config.database {
        builder = builder.db_name(Some(database.clone()));
    }

    let pool = Pool::new(builder).map_err(|e| format!("Failed to create MySQL pool: {e}"))?;
    let mut conn = pool
        .get_conn()
        .map_err(|e| format!("Failed to connect to MySQL: {e}"))?;

    if config.readonly {
        let tx_opts = TxOpts::default().set_access_mode(Some(AccessMode::ReadOnly));
        let mut tx = conn
            .start_transaction(tx_opts)
            .map_err(|e| format!("Failed to start read-only transaction: {e}"))?;
        let output = collect_query_sets(&mut tx, query, max_rows)?;
        tx.rollback()
            .map_err(|e| format!("Failed to close read-only transaction: {e}"))?;
        Ok(output)
    } else {
        collect_query_sets(&mut conn, query, max_rows)
    }
}

fn collect_query_sets<Q: Queryable>(
    queryable: &mut Q,
    query: &str,
    max_rows: usize,
) -> std::result::Result<(Vec<QuerySetOutput>, bool), String> {
    let mut query_result = queryable
        .query_iter(query)
        .map_err(|e| format!("Query failed: {e}"))?;
    let mut total_rows = 0usize;
    let mut truncated = false;
    let mut sets = Vec::new();

    while let Some(mut result_set) = query_result.iter() {
        let columns = result_set
            .columns()
            .as_ref()
            .iter()
            .map(|column| column.name_str().to_string())
            .collect::<Vec<_>>();
        let mut rows = Vec::new();

        while let Some(row_result) = result_set.next() {
            let row = row_result.map_err(|e| format!("Failed to read row: {e}"))?;
            if total_rows >= max_rows {
                truncated = true;
                break;
            }
            rows.push(row_to_json(&row));
            total_rows += 1;
        }

        sets.push(QuerySetOutput {
            columns,
            rows,
            affected_rows: result_set.affected_rows(),
            last_insert_id: result_set.last_insert_id(),
        });

        if truncated {
            break;
        }
    }

    Ok((sets, truncated))
}

fn row_to_json(row: &Row) -> serde_json::Value {
    let mut object = Map::new();
    for (index, column) in row.columns_ref().iter().enumerate() {
        let key = column.name_str().to_string();
        let value = row.as_ref(index).cloned().unwrap_or(MySqlValue::NULL);
        object.insert(key, mysql_value_to_json(value));
    }
    serde_json::Value::Object(object)
}

fn mysql_value_to_json(value: MySqlValue) -> serde_json::Value {
    match value {
        MySqlValue::NULL => serde_json::Value::Null,
        MySqlValue::Bytes(bytes) => {
            serde_json::Value::String(String::from_utf8_lossy(&bytes).into_owned())
        }
        MySqlValue::Int(n) => serde_json::Value::Number(Number::from(n)),
        MySqlValue::UInt(n) => serde_json::Value::Number(Number::from(n)),
        MySqlValue::Float(n) => Number::from_f64(n as f64)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        MySqlValue::Double(n) => Number::from_f64(n)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        MySqlValue::Date(year, month, day, hour, minute, second, micros) => {
            serde_json::Value::String(format!(
                "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{micros:06}"
            ))
        }
        MySqlValue::Time(is_neg, days, hours, minutes, seconds, micros) => {
            let sign = if is_neg { "-" } else { "" };
            serde_json::Value::String(format!(
                "{sign}{days}:{hours:02}:{minutes:02}:{seconds:02}.{micros:06}"
            ))
        }
    }
}

fn validate_readonly_query(query: &str) -> std::result::Result<(), String> {
    let normalized = query.trim();
    if normalized.is_empty() {
        return Err("Query must not be empty.".into());
    }

    let statements = normalized
        .split(';')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    if statements.len() > 1 {
        return Err("Read-only mode only allows a single statement per query.".into());
    }

    let upper = statements[0].trim_start().to_ascii_uppercase();
    let allowed = ["SELECT", "SHOW", "DESCRIBE", "DESC", "EXPLAIN", "WITH"];
    if allowed.iter().any(|prefix| upper.starts_with(prefix)) {
        Ok(())
    } else {
        Err("Read-only mode only allows SELECT, SHOW, DESCRIBE, EXPLAIN, or WITH queries.".into())
    }
}

fn format_result_sets(sets: &[QuerySetOutput], truncated: bool, max_rows: usize) -> String {
    if sets.is_empty() {
        return "Query executed successfully with no result sets.".into();
    }

    let mut out = String::new();
    for (index, set) in sets.iter().enumerate() {
        out.push_str(&format!("Result set {}:\n", index + 1));
        if !set.columns.is_empty() {
            out.push_str(&format!("Columns: {}\n", set.columns.join(", ")));
        }
        out.push_str(&format!("Affected rows: {}\n", set.affected_rows));
        if let Some(last_insert_id) = set.last_insert_id {
            out.push_str(&format!("Last insert id: {}\n", last_insert_id));
        }
        if set.rows.is_empty() {
            out.push_str("Rows: []\n\n");
        } else {
            let pretty_rows =
                serde_json::to_string_pretty(&set.rows).unwrap_or_else(|_| "[]".into());
            out.push_str("Rows:\n");
            out.push_str(&pretty_rows);
            out.push_str("\n\n");
        }
    }

    if truncated {
        out.push_str(&format!("Output truncated after {} rows.\n", max_rows));
    }

    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readonly_defaults_to_true() {
        let yaml = "mysql:\n  host: db.example.com\n  port: 3306\n  password: secret\n";
        let config: ToolsConfig = serde_saphyr::from_str(yaml).unwrap();
        let mysql = config.mysql.unwrap();
        assert!(mysql.readonly);
        assert_eq!(mysql.user, "root");
    }

    #[test]
    fn readonly_rejects_write_queries() {
        let err = validate_readonly_query("UPDATE users SET active = 0").unwrap_err();
        assert!(err.contains("Read-only mode"));
    }

    #[test]
    fn readonly_accepts_select_queries() {
        validate_readonly_query("SELECT * FROM users").unwrap();
        validate_readonly_query("WITH cte AS (SELECT 1) SELECT * FROM cte").unwrap();
    }
}
