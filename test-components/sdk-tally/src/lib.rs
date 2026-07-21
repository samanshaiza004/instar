#![cfg(all(target_os = "wasi", target_env = "p2"))]

use youth_sdk::prelude::*;

struct Tally;

impl Application for Tally {
    fn view(context: &ViewContext) -> Result<Tree> {
        let count = context.state().integer("count")?.unwrap_or(0);
        Ok(Tree::root(BoxNode::column([
            Text::new(node!("count"), format!("Count: {count}")),
            Button::new(node!("increment"), "Increment"),
        ])))
    }

    fn handle(context: &mut EventContext, events: &Events) -> Result<Update> {
        if !events.activated(node!("increment")) {
            return Ok(Update::unchanged());
        }
        let count = context
            .state()
            .integer("count")?
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(Error::internal)?;
        context.state().set_integer("count", count)?;
        Ok(Update::new().set_text(node!("count"), format!("Count: {count}")))
    }
}

youth_sdk::export_app!(Tally);
