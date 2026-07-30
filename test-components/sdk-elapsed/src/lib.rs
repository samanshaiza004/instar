#![cfg(all(target_os = "wasi", target_env = "p2"))]

use youth_sdk::prelude::*;

struct Elapsed;

impl Application for Elapsed {
    fn view(context: &ViewContext) -> Result<Tree> {
        let count = context.state().integer("elapsed-count")?.unwrap_or(0);
        Ok(Tree::root(Text::new(
            node!("status"),
            format!("Elapsed: {count}"),
        )))
    }

    fn handle(context: &mut EventContext, events: &Events) -> Result<Update> {
        let Some((_schedule, _generation, _reason)) = events.elapsed().next() else {
            return Ok(Update::unchanged());
        };
        let count = context
            .state()
            .integer("elapsed-count")?
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(Error::internal)?;
        context.state().set_integer("elapsed-count", count)?;
        Ok(Update::new().set_text(node!("status"), format!("Elapsed: {count}")))
    }
}

youth_sdk::export_app!(Elapsed);
