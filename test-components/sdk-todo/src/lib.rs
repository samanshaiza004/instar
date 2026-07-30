#![cfg(all(target_os = "wasi", target_env = "p2"))]

use youth_sdk::prelude::*;

struct TodoFixture;

impl Application for TodoFixture {
    fn view(context: &ViewContext) -> Result<Tree> {
        let state = context.state();
        let schema = state.integer("model-schema-version")?.unwrap_or(2);
        let first_status = if schema == 1 {
            if state.boolean("todo/1/done")?.unwrap_or(false) {
                "Completed"
            } else {
                "Active"
            }
        } else {
            match state.text("todo/1/status")?.as_deref() {
                Some("completed") => "Completed",
                _ => "Active",
            }
        };
        Ok(Tree::root(Column::new([
            Text::new(node!("summary"), "1 total"),
            Button::command(command!("add"), "Add"),
            Column::named(node!("items"), [row(1, first_status)?]),
        ])))
    }

    fn handle(context: &mut EventContext, events: &Events) -> Result<Update> {
        if !events.commanded(command!("add")) {
            return Err(Error::rejected_event());
        }
        let state = context.state();
        state.set_integer("model-schema-version", 2)?;
        state.set_text("todos-next-id", "3")?;
        state.set_text("todos-order", "1,2")?;
        state.set_text("todo/1/status", "active")?;
        state.delete("todo/1/done")?;
        state.set_text("todo/2/title", "Task 2")?;
        state.set_text("todo/2/status", "active")?;
        Ok(Update::new()
            .insert_child(node!("items"), 1, row(2, "Active")?)
            .set_text(node!("summary"), "2 total"))
    }
}

fn row(id: u64, status: &str) -> Result<Element> {
    let item = ItemKey::new("todo", id)?;
    Ok(Row::named(
        item.node("row")?,
        [
            Text::new(item.node("title")?, format!("Task {id}")),
            Text::new(item.node("status")?, status),
            Button::command(item.command("toggle")?, "Done"),
            Button::command(item.command("delete")?, "Delete"),
        ],
    ))
}

youth_sdk::export_app!(TodoFixture);
