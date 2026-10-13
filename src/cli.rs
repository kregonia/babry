use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    Region,
    Screen,
    Pin(PathBuf),
    PinSurface(PathBuf),
    DebugSelection { source: PathBuf, directory: PathBuf },
    Help,
    OnOutput { command: Box<Command>, name: String },
    ListOutputs,
}

pub fn parse<I>(args: I) -> Result<Command, String>
where
    I: IntoIterator<Item = String>,
{
    let mut values: Vec<String> = args.into_iter().collect();
    if let Some(index) = values.iter().position(|arg| arg == "--output") {
        if index + 1 >= values.len() {
            return Err("--output 需要显示器名称".into());
        }
        let name = values.remove(index + 1);
        values.remove(index);
        if name.is_empty() || name.starts_with("--") || values.iter().any(|arg| arg == "--output") {
            return Err("--output 需要唯一且有效的显示器名称".into());
        }
        let command = parse(values)?;
        if !matches!(
            command,
            Command::Region | Command::Screen | Command::Pin(_) | Command::PinSurface(_)
        ) {
            return Err("--output 仅用于截图或贴图".into());
        }
        return Ok(Command::OnOutput {
            command: Box::new(command),
            name,
        });
    }
    let mut args = values.into_iter();
    let _program = args.next();
    let Some(command) = args.next() else {
        return Ok(Command::Region);
    };

    match command.as_str() {
        "--region" | "--smart" | "--window" | "-r" | "-w" => ensure_no_extra(args, Command::Region),
        "--screen" | "-s" => ensure_no_extra(args, Command::Screen),
        "--pin" => {
            let path = args
                .next()
                .ok_or_else(|| "--pin 需要图片路径".to_string())?;
            ensure_no_extra(args, Command::Pin(PathBuf::from(path)))
        }
        "--pin-surface" => {
            let path = args
                .next()
                .ok_or_else(|| "--pin-surface 需要图片路径".to_string())?;
            ensure_no_extra(args, Command::PinSurface(PathBuf::from(path)))
        }
        "--debug-selection" => {
            let source = args
                .next()
                .ok_or_else(|| "--debug-selection 需要图片路径和输出目录".to_string())?;
            let directory = args
                .next()
                .ok_or_else(|| "--debug-selection 需要输出目录".to_string())?;
            ensure_no_extra(
                args,
                Command::DebugSelection {
                    source: source.into(),
                    directory: directory.into(),
                },
            )
        }
        "--list-outputs" => ensure_no_extra(args, Command::ListOutputs),
        "--help" | "-h" => ensure_no_extra(args, Command::Help),
        unknown => Err(format!("未知参数: {unknown}\n\n{}", usage())),
    }
}

fn ensure_no_extra<I>(mut args: I, command: Command) -> Result<Command, String>
where
    I: Iterator<Item = String>,
{
    if let Some(extra) = args.next() {
        Err(format!("多余参数: {extra}\n\n{}", usage()))
    } else {
        Ok(command)
    }
}

pub fn usage() -> &'static str {
    "用法:\n  babry                    CV 智能区域截图\n  babry --region           CV 智能区域截图\n  babry --smart            CV 智能区域截图\n  babry --screen           全屏截图后进入编辑\n  babry --pin PATH         将 PNG 作为屏幕贴图显示\n  babry --debug-selection IMAGE DIR\n                           输出 CV 候选图和评分 JSON\n  babry --output NAME      在指定显示器截图（可与 --screen 或 --pin 组合）\n  babry --list-outputs     列出显示器及逻辑尺寸、位置、缩放\n  babry --help             显示帮助"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_args(args: &[&str]) -> Result<Command, String> {
        parse(args.iter().map(|arg| (*arg).to_string()))
    }

    #[test]
    fn parses_output_before_or_after_capture_mode() {
        for args in [
            vec!["babry", "--output", "DP-1", "--screen"],
            vec!["babry", "--screen", "--output", "DP-1"],
        ] {
            assert_eq!(
                parse_args(&args),
                Ok(Command::OnOutput {
                    command: Box::new(Command::Screen),
                    name: "DP-1".into()
                })
            );
        }
        assert!(parse_args(&["babry", "--output"]).is_err());
        assert!(parse_args(&["babry", "--output", "DP-1", "--output", "DP-2"]).is_err());
        assert!(parse_args(&["babry", "--list-outputs", "--output", "DP-1"]).is_err());
    }

    #[test]
    fn defaults_to_region_capture() {
        assert_eq!(parse_args(&["babry"]), Ok(Command::Region));
    }

    #[test]
    fn parses_smart_capture_aliases() {
        assert_eq!(parse_args(&["babry", "--smart"]), Ok(Command::Region));
        assert_eq!(parse_args(&["babry", "--window"]), Ok(Command::Region));
    }

    #[test]
    fn rejects_removed_longshot_commands() {
        assert!(parse_args(&["babry", "--longshot"]).is_err());
        assert!(parse_args(&["babry", "--stitch", "out.png", "frame.png"]).is_err());
    }

    #[test]
    fn parses_selection_diagnostics_and_requires_both_paths() {
        assert_eq!(
            parse_args(&["babry", "--debug-selection", "image.png", "debug"]),
            Ok(Command::DebugSelection {
                source: "image.png".into(),
                directory: "debug".into()
            })
        );
        assert!(parse_args(&["babry", "--debug-selection", "image.png"]).is_err());
        assert!(parse_args(&["babry", "--debug-selection"]).is_err());
        assert!(
            parse_args(&["babry", "--debug-selection", "image.png", "debug", "extra"]).is_err()
        );
    }

    #[test]
    fn parses_pin_surface_command() {
        assert_eq!(
            parse_args(&["babry", "--pin-surface", "image.png"]),
            Ok(Command::PinSurface(PathBuf::from("image.png")))
        );
    }
}
