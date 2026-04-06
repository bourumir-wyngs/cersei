//! CAS (Computer Algebra System) tool: symbolic math via giac/xcas.

use super::*;
use serde::Deserialize;
use std::sync::Mutex;

/// Global mutex to serialize giac calls — the C++ library is not thread-safe.
static GIAC_LOCK: once_cell::sync::Lazy<Mutex<()>> = once_cell::sync::Lazy::new(|| Mutex::new(()));

pub struct CasTool;

#[async_trait]
impl Tool for CasTool {
    fn name(&self) -> &str {
        "CAS"
    }

    fn description(&self) -> &str {
        "Evaluate symbolic math expressions using the giac/xcas computer algebra system. \
         Supports algebra (factor, simplify, solve), calculus (integrate, diff, limit, series), \
         linear algebra (det, inv, eigenvals), number theory (ifactor, gcd, isprime), \
         and more. Input is any valid giac expression string."
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::None
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::Custom
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "expression": {
                    "type": "string",
                    "description": "A giac/xcas expression to evaluate. Examples: \
                        \"factor(x^4-1)\", \"integrate(sin(x),x)\", \"det([[1,2],[3,4]])\", \
                        \"solve(x^2+2*x-3=0,x)\", \"diff(x^3*sin(x),x)\", \"ifactor(1234567)\""
                },
                "epsilon": {
                    "type": "number",
                    "description": "Precision for numeric computations (optional). Smaller values give higher precision."
                }
            },
            "required": ["expression"]
        })
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> ToolResult {
        #[derive(Deserialize)]
        struct Input {
            expression: String,
            epsilon: Option<f64>,
        }

        let input: Input = match serde_json::from_value(input) {
            Ok(i) => i,
            Err(e) => return ToolResult::error(format!("Invalid input: {e}")),
        };

        if input.expression.trim().is_empty() {
            return ToolResult::error("Expression cannot be empty");
        }

        // Run in a blocking task since giac is a C++ library
        let expression = input.expression;
        let epsilon = input.epsilon;

        let result = tokio::task::spawn_blocking(move || {
            // Serialize all giac access — the C++ library is not thread-safe
            let _guard = GIAC_LOCK.lock().unwrap_or_else(|e| e.into_inner());

            let mut ctx = giacrs::context::Context::new();

            if let Some(eps) = epsilon {
                ctx.set_epsilon(eps);
            }

            let eval_result = ctx.eval(&expression);

            // Convert Gen to string before dropping context
            match eval_result {
                Ok(gen) => {
                    let output = gen.to_string();
                    drop(gen);
                    drop(ctx);
                    Ok(output)
                }
                Err(e) => {
                    drop(ctx);
                    Err(format!("giac error: {e:?}"))
                }
            }
        })
        .await;

        match result {
            Ok(Ok(output)) => ToolResult::success(output),
            Ok(Err(e)) => ToolResult::error(e),
            Err(e) => ToolResult::error(format!("Task join error: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_schema() {
        let tool = CasTool;
        assert_eq!(tool.name(), "CAS");
        assert!(tool.input_schema()["properties"]["expression"].is_object());
        assert_eq!(tool.permission_level(), PermissionLevel::None);
    }

    fn make_ctx() -> ToolContext {
        ToolContext {
            working_dir: std::path::PathBuf::from("/tmp"),
            session_id: "test".to_string(),
            permissions: Arc::new(permissions::AllowAll),
            cost_tracker: Arc::new(CostTracker::new()),
            mcp_manager: None,
            extensions: Extensions::default(),
        }
    }

    #[tokio::test]
    async fn test_empty_expression() {
        let result = CasTool
            .execute(serde_json::json!({ "expression": "  " }), &make_ctx())
            .await;
        assert!(result.is_error);
    }

    #[tokio::test]
    async fn test_eval_basic() {
        let result = CasTool
            .execute(serde_json::json!({ "expression": "1+1" }), &make_ctx())
            .await;
        assert!(!result.is_error);
        assert_eq!(result.content.trim(), "2");
    }

    #[tokio::test]
    async fn test_eval_factor() {
        let result = CasTool
            .execute(
                serde_json::json!({ "expression": "factor(x^2-1)" }),
                &make_ctx(),
            )
            .await;
        assert!(!result.is_error);
        assert!(result.content.contains("x"));
    }
}
