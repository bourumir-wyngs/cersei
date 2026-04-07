//! PostgreSQL query tool driven by runtime configuration loaded from `tools.yaml`.

use super::*;
use postgres::{Client, NoTls, SimpleQueryMessage};
use serde::Deserialize;
use serde_json::Map;

pub struct PostgresTool;

#[derive(Debug, Deserialize)]
struct Input {
    query: String,
    max_rows: Option<usize>,
}

#[derive(Debug, Default)]
struct QuerySetOutput {
    columns: Vec<String>,
    rows: Vec<serde_json::Value>,
    affected_rows: Option<u64>,
}

#[async_trait]
impl Tool for PostgresTool {
    fn name(&self) -> &str {
        "PostgreSql"
    }

    fn description(&self) -> &str {
        "Execute a SQL query against the PostgreSQL database configured in tools.yaml."
    }

    fn permission_level(&self) -> PermissionLevel {
        match global_tools_config().postgresql {
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
                    "description": "SQL query to execute against the configured PostgreSQL database"
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
            None => return ToolResult::error(
                "PostgreSQL is not configured. Add a tools.yaml file with a postgresql section.",
            ),
        };
        let config = match &tools_config.postgresql {
            Some(cfg) => cfg.clone(),
            None => return ToolResult::error("PostgreSQL is not configured in tools.yaml."),
        };

        if config.readonly {
            if let Err(e) = validate_readonly_query(&input.query) {
                return ToolResult::error(e);
            }
        }

        let max_rows = input.max_rows.unwrap_or(100).clamp(1, 1000);
        let query = input.query;
        let result =
            tokio::task::spawn_blocking(move || execute_postgres_query(&config, &query, max_rows))
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
            Err(e) => ToolResult::error(format!("PostgreSQL task failed: {e}")),
        }
    }
}

fn execute_postgres_query(
    config: &PostgresToolConfig,
    query: &str,
    max_rows: usize,
) -> std::result::Result<(Vec<QuerySetOutput>, bool), String> {
    let mut params = vec![
        format!("host={}", config.host),
        format!("port={}", config.port),
        format!("user={}", quote_conn_value(&config.user)),
        format!("password={}", quote_conn_value(&config.password)),
    ];
    if let Some(database) = &config.database {
        params.push(format!("dbname={}", quote_conn_value(database)));
    }

    let mut client = Client::connect(&params.join(" "), NoTls)
        .map_err(|e| format!("Failed to connect to PostgreSQL: {e}"))?;

    if config.readonly {
        let mut tx = client
            .build_transaction()
            .read_only(true)
            .start()
            .map_err(|e| format!("Failed to start read-only transaction: {e}"))?;
        let output = collect_query_sets(&mut tx, query, max_rows)?;
        tx.rollback()
            .map_err(|e| format!("Failed to close read-only transaction: {e}"))?;
        Ok(output)
    } else {
        collect_query_sets(&mut client, query, max_rows)
    }
}

fn collect_query_sets<C>(
    client: &mut C,
    query: &str,
    max_rows: usize,
) -> std::result::Result<(Vec<QuerySetOutput>, bool), String>
where
    C: postgres::GenericClient,
{
    let messages = client
        .simple_query(query)
        .map_err(|e| format!("Query failed: {e}"))?;

    let mut sets = Vec::new();
    let mut current = QuerySetOutput::default();
    let mut total_rows = 0usize;
    let mut truncated = false;

    for message in messages {
        match message {
            SimpleQueryMessage::Row(row) => {
                if current.columns.is_empty() {
                    current.columns = row
                        .columns()
                        .iter()
                        .map(|column| column.name().to_string())
                        .collect();
                }
                if total_rows >= max_rows {
                    truncated = true;
                    break;
                }
                current.rows.push(simple_row_to_json(&row));
                total_rows += 1;
            }
            SimpleQueryMessage::CommandComplete(count) => {
                current.affected_rows = Some(count);
                sets.push(std::mem::take(&mut current));
            }
            _ => {}
        }
    }

    if !current.columns.is_empty() || !current.rows.is_empty() || current.affected_rows.is_some() {
        sets.push(current);
    }

    Ok((sets, truncated))
}

fn simple_row_to_json(row: &postgres::SimpleQueryRow) -> serde_json::Value {
    let mut object = Map::new();
    for (idx, column) in row.columns().iter().enumerate() {
        let key = column.name().to_string();
        let value = row
            .get(idx)
            .map(|v| serde_json::Value::String(v.to_string()))
            .unwrap_or(serde_json::Value::Null);
        object.insert(key, value);
    }
    serde_json::Value::Object(object)
}

fn quote_conn_value(value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('\'', "\\'");
    format!("'{}'", escaped)
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
    let allowed = ["SELECT", "SHOW", "EXPLAIN", "WITH"];
    if allowed.iter().any(|prefix| upper.starts_with(prefix)) {
        Ok(())
    } else {
        Err("Read-only mode only allows SELECT, SHOW, EXPLAIN, or WITH queries.".into())
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
        if let Some(affected_rows) = set.affected_rows {
            out.push_str(&format!("Affected rows: {}\n", affected_rows));
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
        let yaml = "postgresql:\n  host: db.example.com\n  port: 5432\n  password: secret\n";
        let config: ToolsConfig = serde_saphyr::from_str(yaml).unwrap();
        let postgres = config.postgresql.unwrap();
        assert!(postgres.readonly);
        assert_eq!(postgres.user, "postgres");
    }

    #[test]
    fn readonly_rejects_write_queries() {
        let err = validate_readonly_query("DELETE FROM users").unwrap_err();
        assert!(err.contains("Read-only mode"));
    }
}
