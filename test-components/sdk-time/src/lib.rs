//! SDK-backed fixture that creates a durable schedule through `EventContext::time`.

#![cfg(all(target_os = "wasi", target_env = "p2"))]

use std::time::Duration;

use youth_sdk::prelude::*;

struct TimeApp;

impl Application for TimeApp {
    fn view(_context: &ViewContext) -> Result<Tree> {
        Ok(Tree::root(Column::new([
            Text::new(node!("status"), "ready"),
            Button::new(node!("schedule"), "Schedule"),
        ])))
    }

    fn handle(context: &mut EventContext, events: &Events) -> Result<Update> {
        if !events.activated(node!("schedule")) {
            return Ok(Update::unchanged());
        }
        let options =
            ScheduleOptions::new().notification(Notification::new("Youth timer", "Time elapsed"));
        context
            .time()
            .schedule_after(Duration::from_secs(1), options)
            .map_err(|_| Error::internal().with_message("schedule creation failed"))?;
        if context.state().bytes("__schedule_storage_probe")?.is_some() {
            return Err(Error::internal()
                .with_message("schedule storage leaked through the state key-value interface"));
        }
        Ok(Update::new().set_text(node!("status"), "scheduled"))
    }
}

youth_sdk::export_app!(TimeApp);
