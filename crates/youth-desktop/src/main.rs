use std::path::PathBuf;
use std::process::ExitCode;

use youth_desktop::{DesktopOptions, run, window_smoke};
use youth_runtime::{AppId, RuntimeLimits, StateLocation, YouthAppConfig};

fn main() -> ExitCode {
    let arguments = std::env::args_os().skip(1).collect::<Vec<_>>();
    let result = if arguments.len() == 1 && arguments[0] == "--window-smoke" {
        window_smoke()
    } else {
        parse_options(&arguments).and_then(run)
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

fn parse_options(
    arguments: &[std::ffi::OsString],
) -> Result<DesktopOptions, youth_desktop::DesktopError> {
    let component = arguments.first().map(PathBuf::from).ok_or_else(|| {
        youth_desktop::DesktopError::Arguments("expected a component path or --window-smoke".into())
    })?;
    let mut app_id = None;
    let mut state = StateLocation::Memory;
    let mut index = 1;
    while index < arguments.len() {
        match arguments[index].to_str() {
            Some("--app-id") => {
                index += 1;
                let value = arguments
                    .get(index)
                    .and_then(|value| value.to_str())
                    .ok_or_else(|| {
                        youth_desktop::DesktopError::Arguments(
                            "--app-id requires UTF-8 text".into(),
                        )
                    })?;
                app_id =
                    Some(AppId::parse(value).map_err(|error| {
                        youth_desktop::DesktopError::Arguments(error.to_string())
                    })?);
            }
            Some("--state-file") => {
                index += 1;
                let path = arguments.get(index).map(PathBuf::from).ok_or_else(|| {
                    youth_desktop::DesktopError::Arguments("--state-file requires a path".into())
                })?;
                state = StateLocation::File(path);
            }
            _ => {
                return Err(youth_desktop::DesktopError::Arguments(
                    "unknown youth-desktop argument".into(),
                ));
            }
        }
        index += 1;
    }
    Ok(DesktopOptions {
        config: YouthAppConfig {
            component_path: component,
            app_id: app_id.ok_or_else(|| {
                youth_desktop::DesktopError::Arguments("--app-id is required".into())
            })?,
            state,
            limits: RuntimeLimits::default(),
            workspace: None,
        },
        width: 640,
        height: 360,
    })
}
