use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    Region,
    Screen,
    Pin(PathBuf),
    PinSurface(PathBuf),
    Help,
}

pub fn parse<I>(args: I) -> Result<Command, String>
where
    I: IntoIterator<Item = String>,
{
    let mut args = args.into_iter();
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
    "用法:\n  babry                    CV 智能区域截图\n  babry --region           CV 智能区域截图\n  babry --smart            CV 智能区域截图\n  babry --screen           全屏截图后进入编辑\n  babry --pin PATH         将 PNG 作为屏幕贴图显示\n  babry --help             显示帮助"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_args(args: &[&str]) -> Result<Command, String> {
        parse(args.iter().map(|arg| (*arg).to_string()))
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
    fn parses_pin_surface_command() {
        assert_eq!(
            parse_args(&["babry", "--pin-surface", "image.png"]),
            Ok(Command::PinSurface(PathBuf::from("image.png")))
        );
    }
}
