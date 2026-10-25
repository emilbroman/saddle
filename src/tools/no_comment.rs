use futures::future::BoxFuture;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{Tool, ToolOutput, parse_args};

pub struct NoComment;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {}

impl Tool for NoComment {
    fn name(&self) -> &'static str {
        "no_comment"
    }

    fn description(&self) -> &'static str {
        "End an automatic file/diagnostic/git-event turn with nothing to say. Call alone, with no text and no arguments. Never use for the user's own messages."
    }

    fn schema(&self) -> Value {
        json!({"type": "object", "properties": {}, "additionalProperties": false})
    }

    fn summary(&self, _: &Value) -> String {
        String::new()
    }

    fn call(&self, args: Value) -> BoxFuture<'static, ToolOutput> {
        Box::pin(async move {
            match parse_args::<Args>(args) {
                Ok(_) => ToolOutput::ok("Automatic event acknowledged without comment."),
                Err(error) => error,
            }
        })
    }
}
