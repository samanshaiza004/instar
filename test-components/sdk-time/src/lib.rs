//! SDK-backed fixture that handles the host's scheduling stub without trapping.

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
        match context
            .time()
            .schedule_after(Duration::from_secs(1), options)
        {
            Err(_) => Ok(Update::new().set_text(node!("status"), "unavailable")),
            Ok(_) => Err(Error::internal()
                .with_message("the Developer Preview 2 scheduling stub unexpectedly succeeded")),
        }
    }
}

youth_sdk::export_app!(TimeApp);
