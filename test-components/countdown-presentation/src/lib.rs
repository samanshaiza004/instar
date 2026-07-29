#![cfg(all(target_os = "wasi", target_env = "p2"))]
use std::time::Duration;
use youth_sdk::prelude::*;
struct CountdownApp;
impl Application for CountdownApp {
    fn view(context: &ViewContext) -> Result<Tree> {
        let remaining = match context.state().schedule("active")? {
            Some(schedule) => Countdown::new(node!("remaining"), schedule),
            None => Text::new(node!("remaining"), "--:--"),
        };
        Ok(Tree::root(Column::new([
            remaining,
            Button::new(node!("start"), "Start"),
        ])))
    }
    fn handle(context: &mut EventContext, events: &Events) -> Result<Update> {
        if events.elapsed().next().is_some() {
            let count = context.state().integer("elapsed-count")?.unwrap_or(0) + 1;
            context.state().set_integer("elapsed-count", count)?;
            return Ok(Update::unchanged());
        }
        let schedule = context
            .time()
            .schedule_after(Duration::from_secs(300), ScheduleOptions::new())?;
        context.state().set_schedule("active", schedule)?;
        Ok(Update::unchanged())
    }
}
youth_sdk::export_app!(CountdownApp);
