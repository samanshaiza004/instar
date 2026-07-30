//! Protocol 0.0.4 scheduling plumbing and coexistence regressions.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{counter_component, test_component};
use tempfile::tempdir;
use youth_runtime::{
    AppId, AppLifecycle, RuntimeLimits, RuntimeTimeSeams, ScheduleWake, StateLocation,
    VirtualDeadlineClock, VirtualGuestMonotonicClock, VirtualWakeDriver, WakeDisposition, YouthApp,
    YouthAppConfig,
};
use youth_state::{GuestCallPhase, StateLimits, StateStore};
use youth_tree::{NodeData, NodeId};

#[test]
fn time_component_loads_reports_v004_and_mounts() {
    let component = test_component("youth-time-stub");
    let mut app = YouthApp::load(component).expect("time component loads");

    assert_eq!(app.inspect().world, "youth:app/application@0.0.4");
    let snapshot = app.mount().expect("time component mounts");
    assert_eq!(snapshot.revision, 0);
    assert_eq!(snapshot.nodes.len(), 6);
    assert_eq!(app.lifecycle(), AppLifecycle::Mounted);
}

#[test]
fn direct_time_component_creates_a_schedule_without_faulting() {
    let mut app = YouthApp::load(test_component("youth-time-stub")).expect("time component loads");
    app.mount().expect("time component mounts");

    let receipt = app
        .activate(NodeId::new(4).expect("fixture node ID is valid"))
        .expect("schedule creation commits");

    assert!(receipt.committed);
    assert_eq!(app.lifecycle(), AppLifecycle::Mounted);
    let snapshot = app.snapshot().expect("mounted snapshot is available");
    assert!(snapshot.nodes.iter().any(|node| {
        matches!(
            &node.data,
            NodeData::Text { value } if value == "scheduled"
        )
    }));
}

#[test]
fn v002_v003_and_v004_components_coexist() {
    let mut counter = YouthApp::load(counter_component()).expect("v0.0.2 counter loads");
    assert_eq!(counter.inspect().world, "youth:app/application@0.0.2");
    counter.mount().expect("v0.0.2 counter mounts");

    let mut legacy =
        YouthApp::load(test_component("youth-legacy-v003")).expect("v0.0.3 legacy guest loads");
    assert_eq!(legacy.inspect().world, "youth:app/application@0.0.3");
    legacy.mount().expect("v0.0.3 legacy guest mounts");

    // youth-sdk-tally and youth-sdk-time are local-path youth-sdk fixtures,
    // not raw wit-bindgen guests like youth-legacy-v003/youth-time-stub --
    // they always track whatever protocol the SDK crate in this workspace
    // currently targets (0.0.5, since Gate C-3), not a version frozen at
    // this file's original name. The point of this test is that multiple
    // *distinct* protocol worlds coexist in one runtime, which still holds
    // regardless of which one the SDK's own current version happens to be.
    let mut tally =
        YouthApp::load(test_component("youth-sdk-tally")).expect("current SDK tally loads");
    assert_eq!(tally.inspect().world, "youth:app/application@0.0.5");
    tally.mount().expect("current SDK tally mounts");

    let mut sdk_time =
        YouthApp::load(test_component("youth-sdk-time")).expect("current SDK time guest loads");
    assert_eq!(sdk_time.inspect().world, "youth:app/application@0.0.5");
    sdk_time.mount().expect("current SDK time guest mounts");
}

fn sdk_time_config(database: &std::path::Path) -> YouthAppConfig {
    YouthAppConfig {
        component_path: test_component("youth-sdk-time"),
        app_id: AppId::parse("dev.youth.time").unwrap(),
        state: StateLocation::File(database.to_owned()),
        limits: RuntimeLimits::default(),
    }
}

#[test]
fn sdk_time_schedule_survives_a_real_runtime_restart_and_is_hidden_from_state_get() {
    let directory = tempdir().unwrap();
    let database = directory.path().join("state.sqlite3");
    {
        let mut app = YouthApp::load_config(sdk_time_config(&database))
            .expect("first SDK time runtime loads");
        let snapshot = app.mount().expect("SDK time component mounts");
        let button = snapshot
            .nodes
            .iter()
            .find(|node| {
                matches!(
                    &node.data,
                    NodeData::Button { label, .. } if label == "Schedule"
                )
            })
            .expect("fixture has a schedule button")
            .id;
        let receipt = app.activate(button).expect("SDK schedule creation commits");
        assert!(receipt.committed);
        assert!(app.snapshot().expect("snapshot").nodes.iter().any(|node| {
            matches!(
                &node.data,
                NodeData::Text { value } if value == "scheduled"
            )
        }));
    }

    {
        let mut restarted = YouthApp::load_config(sdk_time_config(&database))
            .expect("second SDK time runtime loads against the same state file");
        restarted
            .mount()
            .expect("restarted SDK time component mounts");
    }

    let mut store =
        StateStore::open(StateLocation::File(database), StateLimits::default()).unwrap();
    let schedules = store.schedules().unwrap();
    assert_eq!(schedules.len(), 1);
    assert_eq!(
        schedules[0].notification,
        Some(("Youth timer".into(), "Time elapsed".into()))
    );
    store.begin(GuestCallPhase::Resync).unwrap();
    assert_eq!(store.get("__schedule_storage_probe").unwrap(), None);
    store.rollback().unwrap();
}

#[test]
fn runtime_open_arms_future_schedules_without_invoking_the_guest() {
    let directory = tempdir().unwrap();
    let database = directory.path().join("state.sqlite3");
    let mut store = StateStore::open(
        StateLocation::File(database.clone()),
        StateLimits::default(),
    )
    .unwrap();
    store.begin(GuestCallPhase::Handle).unwrap();
    let created = store.schedule_create(1_000, 500, None).unwrap();
    store.commit().unwrap();
    drop(store);

    let deadline = VirtualDeadlineClock::new(1_000);
    let wakes = VirtualWakeDriver::default();
    let mut config = sdk_time_config(&database);
    config.limits.time = RuntimeTimeSeams {
        deadline_clock: Arc::new(deadline.clone()),
        wake_driver: Arc::new(wakes.clone()),
        guest_monotonic_clock: Arc::new(VirtualGuestMonotonicClock::new(0)),
        ..RuntimeTimeSeams::default()
    };
    let mut app = YouthApp::load_config(config).expect("runtime opens without mounting the guest");
    assert_eq!(
        wakes.armed(),
        vec![(
            youth_runtime::WakeToken::for_record(AppId::parse("dev.youth.time").unwrap(), &created,),
            Duration::from_millis(500)
        )]
    );
    assert_eq!(wakes.arm_count(), 1);
    assert_eq!(app.lifecycle(), AppLifecycle::Loaded);
    assert!(app.tree().is_none());

    let wrong_app = ScheduleWake {
        application_id: AppId::parse("dev.youth.someone-else").unwrap(),
        token: youth_runtime::WakeToken::for_record(
            AppId::parse("dev.youth.time").unwrap(),
            &created,
        ),
    };
    assert_eq!(
        app.receive_schedule_wake(&wrong_app).unwrap(),
        WakeDisposition::Discarded
    );
    deadline.advance(Duration::from_millis(500));
    wakes.advance(Duration::from_millis(500));
    let token = wakes.due().pop().expect("the virtual wake is due");
    assert!(wakes.fire(&token));
    let wake = ScheduleWake {
        application_id: AppId::parse("dev.youth.time").unwrap(),
        token: wakes.take_received().pop().unwrap(),
    };
    assert_eq!(
        app.receive_schedule_wake(&wake).unwrap(),
        WakeDisposition::DeliveryQueued
    );
    assert_eq!(app.pending_deliveries().unwrap().len(), 1);
    assert_eq!(app.lifecycle(), AppLifecycle::Loaded);
    assert!(app.tree().is_none());
}

#[test]
fn guest_instant_now_uses_only_the_injected_monotonic_clock() {
    let guest_clock = VirtualGuestMonotonicClock::new(5_000_000_000).with_step(42_000_000);
    let mut config = YouthAppConfig::ephemeral(test_component("youth-instant-now"));
    config.limits.time = RuntimeTimeSeams {
        deadline_clock: Arc::new(VirtualDeadlineClock::new(0)),
        wake_driver: Arc::new(VirtualWakeDriver::default()),
        guest_monotonic_clock: Arc::new(guest_clock),
        ..RuntimeTimeSeams::default()
    };
    let mut app = YouthApp::load_config(config).expect("instant fixture loads");
    let snapshot = app.mount().expect("instant fixture mounts");
    assert!(
        snapshot
            .nodes
            .iter()
            .any(|node| { matches!(&node.data, NodeData::Text { value } if value == "42000000") })
    );
}
