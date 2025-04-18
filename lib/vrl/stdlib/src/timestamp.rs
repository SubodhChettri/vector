use vrl::prelude::*;

fn timestamp(value: Value) -> Resolved {
    match value {
        v @ Value::Timestamp(_) => Ok(v),
        v => Err(format!(r#"expected timestamp, got {}"#, v.kind()).into()),
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Timestamp;

impl Function for Timestamp {
    fn identifier(&self) -> &'static str {
        "timestamp"
    }

    fn parameters(&self) -> &'static [Parameter] {
        &[Parameter {
            keyword: "value",
            kind: kind::ANY,
            required: true,
        }]
    }

    fn examples(&self) -> &'static [Example] {
        &[
            Example {
                title: "valid",
                source: r#"to_string(timestamp(t'2021-02-11 21:42:01Z'))"#,
                result: Ok(r#""2021-02-11T21:42:01Z""#),
            },
            Example {
                title: "invalid",
                source: "timestamp!(true)",
                result: Err(
                    r#"function call error for "timestamp" at (0:16): expected timestamp, got boolean"#,
                ),
            },
        ]
    }

    fn compile(
        &self,
        _state: &state::Compiler,
        _ctx: &mut FunctionCompileContext,
        mut arguments: ArgumentList,
    ) -> Compiled {
        let value = arguments.required("value");

        Ok(Box::new(TimestampFn { value }))
    }

    fn call_by_vm(&self, _ctx: &mut Context, args: &mut VmArgumentList) -> Resolved {
        let value = args.required("value");
        timestamp(value)
    }
}

#[derive(Debug, Clone)]
struct TimestampFn {
    value: Box<dyn Expression>,
}

impl Expression for TimestampFn {
    fn resolve(&self, ctx: &mut Context) -> Resolved {
        let value = self.value.resolve(ctx)?;
        timestamp(value)
    }

    fn type_def(&self, state: &state::Compiler) -> TypeDef {
        let non_timestamp = !self.value.type_def(state).is_timestamp();

        TypeDef::timestamp().with_fallibility(non_timestamp)
    }
}
