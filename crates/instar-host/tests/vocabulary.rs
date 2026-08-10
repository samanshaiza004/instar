//! The consumer half of the input vocabulary's coverage rule.
//!
//! `instar-window` asserts that every [`WindowOutput`] term is *produced* by
//! `winit_adapter::translate`. This asserts the other half: that every term is
//! *consumed* by `Host::handle`. Together they close the seam.
//!
//! ```text
//! winit event
//!    ↓
//! WindowOutput          <- every term produced   (instar-window/tests/layering.rs)
//!    ↓
//! host routing          <- every term consumed   (here)
//!    ↓
//! observable behaviour  <- the Gallery's job
//! ```
//!
//! Only seam enums get this treatment, and only where ignoring a variant is
//! legal. `WindowOutput` qualifies twice over: a `match` arm returning `None`
//! on the producing side and a `_ => {}` on the consuming side are both
//! perfectly well-typed, and three subsystems were disconnected that way --
//! the wheel, the pointer move, and the keyboard -- each complete and tested
//! on both sides of a seam that did not exist.
//!
//! Neither test is the real proof. A lexical check can always acquire another
//! false-positive shape; the first draft of the producer test was green with
//! the defect present, because the adapter's own test module named the term it
//! was searching for. The runtime proof is the Gallery, where a real gesture
//! has to produce the right pixels. These two are the backstop that fails
//! fast when someone adds a term and forgets a side.

/// Every input term the window layer can emit is routed by the host.
#[test]
fn every_window_output_term_is_consumed_by_the_host() {
    let window = include_str!("../../instar-window/src/lib.rs");
    // Production code only, for the reason the producing side learned the hard
    // way: a test module naming the term satisfies a search for the arm that
    // test exists to check.
    let host = include_str!("../src/lib.rs")
        .split_once("\n#[cfg(test)]\nmod tests {")
        .map_or_else(
            || panic!("instar-host's test module marks the end of production code"),
            |(production, _)| production,
        );

    let body = window
        .split_once("pub enum WindowOutput {")
        .expect("WindowOutput is declared in instar-window")
        .1
        .split_once("\n}")
        .expect("the enum ends")
        .0;

    let terms: Vec<&str> = body
        .lines()
        .map(str::trim)
        .filter(|line| line.chars().next().is_some_and(char::is_uppercase))
        .map(|line| {
            line.split(['(', ' ', '{', ','])
                .next()
                .expect("a variant name")
        })
        .collect();

    assert!(
        terms.len() >= 7,
        "only found {terms:?} -- the parser stopped matching the enum's shape"
    );

    let missing: Vec<&&str> = terms
        .iter()
        .filter(|term| !host.contains(&format!("WindowOutput::{term}")))
        .collect();

    assert!(
        missing.is_empty(),
        "these input terms exist, and the winit adapter produces them, but \
         nothing in instar-host ever matches them: {missing:?}\n\n\
         A term with a producer and no consumer is input the application \
         receives and silently discards, which is the same defect as a term \
         with no producer, seen from the other end."
    );
}
