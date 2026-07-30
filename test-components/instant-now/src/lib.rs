//! Surfaces the interval between two guest `Instant::now()` observations.

#![cfg(all(target_os = "wasi", target_env = "p2"))]

use std::time::Instant;

use youth_sdk::prelude::*;

struct InstantNowApp;

impl Application for InstantNowApp {
    fn view(_context: &ViewContext) -> Result<Tree> {
        let started = Instant::now();
        let elapsed = started.elapsed().as_nanos();
        Ok(Tree::root(Text::new(node!("reading"), elapsed.to_string())))
    }

    fn handle(_context: &mut EventContext, _events: &Events) -> Result<Update> {
        Ok(Update::unchanged())
    }
}

youth_sdk::export_app!(InstantNowApp);
