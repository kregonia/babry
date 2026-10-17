use std::{
    env, fs,
    path::{Path, PathBuf},
};

/// 每次截图请求读取配置，后台运行期间修改默认目录也能在下次唤起生效。
pub fn resolve_save_dir(cli_dir: Option<&Path>) -> Result<PathBuf, String> {
    SavePaths::from_env()?.resolve(cli_dir)
}

struct SavePaths {
    home: Option<PathBuf>,
    config: Option<PathBuf>,
    env_save_dir: Option<PathBuf>,
    pictures: Option<PathBuf>,
    cwd: PathBuf,
}

impl SavePaths {
    fn from_env() -> Result<Self, String> {
        let home = nonempty_env_path("HOME");
        let config_home = nonempty_env_path("XDG_CONFIG_HOME")
            .filter(|path| path.is_absolute())
            .or_else(|| home.as_ref().map(|home| home.join(".config")));
        Ok(Self {
            home,
            config: config_home.map(|base| base.join("babry/config.toml")),
            env_save_dir: nonempty_env_path("BABRY_SAVE_DIR"),
            pictures: nonempty_env_path("XDG_PICTURES_DIR"),
            cwd: env::current_dir().map_err(|error| format!("读取当前目录失败: {error}"))?,
        })
    }

    fn resolve(&self, cli_dir: Option<&Path>) -> Result<PathBuf, String> {
        let path = if let Some(path) = cli_dir.or(self.env_save_dir.as_deref()) {
            expand_path(path, self.home.as_deref(), &self.cwd)?
        } else if let Some((path, base)) = self.read_config()? {
            // 配置中的相对路径以配置文件目录为基准，不受快捷键/后台的工作目录影响。
            expand_path(&path, self.home.as_deref(), &base)?
        } else {
            let pictures = self
                .pictures
                .clone()
                .or_else(|| self.home.as_ref().map(|home| home.join("Pictures")))
                .unwrap_or_else(|| self.cwd.clone());
            expand_path(&pictures, self.home.as_deref(), &self.cwd)?
        };
        match fs::metadata(&path) {
            Ok(metadata) if !metadata.is_dir() => {
                return Err(format!("保存路径不是目录: {}", path.display()));
            }
            Ok(_) => (),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => return Err(format!("无法访问保存目录 {}: {error}", path.display())),
        }
        // 选择或取消截图不创建目录，真正导出时才按需创建。
        Ok(path)
    }

    fn read_config(&self) -> Result<Option<(PathBuf, PathBuf)>, String> {
        let Some(path) = &self.config else {
            return Ok(None);
        };
        let content = match fs::read_to_string(path) {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(format!("读取配置失败 {}: {error}", path.display())),
        };
        let table: toml::Table = content
            .parse()
            .map_err(|error| format!("解析配置失败 {}: {error}", path.display()))?;
        let Some(value) = table.get("save_dir") else {
            return Ok(None);
        };
        let directory = value
            .as_str()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| format!("配置 {} 的 save_dir 必须是非空字符串", path.display()))?;
        Ok(Some((
            directory.into(),
            path.parent()
                .expect("config file has a directory")
                .to_path_buf(),
        )))
    }
}

fn nonempty_env_path(key: &str) -> Option<PathBuf> {
    env::var_os(key)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn expand_path(path: &Path, home: Option<&Path>, base: &Path) -> Result<PathBuf, String> {
    if path.as_os_str().is_empty() {
        return Err("保存目录不能为空".into());
    }
    let relative_home = ["~", "$HOME", "${HOME}"]
        .iter()
        .find_map(|prefix| path.strip_prefix(prefix).ok());
    let path = if let Some(rest) = relative_home {
        home.ok_or("保存目录使用了主目录缩写，但 HOME 未设置")?
            .join(rest)
    } else {
        path.to_path_buf()
    };
    Ok(if path.is_absolute() {
        path
    } else {
        base.join(path)
    })
}

/// 为保存、复制和贴图统一生成路径。创建失败会交给界面报告，不悄悄切到其他目录。
pub fn output_path(directory: &Path) -> Result<PathBuf, String> {
    fs::create_dir_all(directory)
        .map_err(|error| format!("无法创建保存目录 {}: {error}", directory.display()))?;
    Ok(directory.join(format!(
        "babry-{}.png",
        chrono::Local::now().format("%Y%m%d-%H%M%S-%3f")
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(root: &Path) -> SavePaths {
        SavePaths {
            home: Some(root.join("home")),
            config: Some(root.join("config/babry/config.toml")),
            env_save_dir: None,
            pictures: None,
            cwd: root.join("working"),
        }
    }

    #[test]
    fn priority_expansion_and_config_reload_are_independent_of_daemon_working_directory() {
        let root = tempfile::tempdir().unwrap();
        let mut paths = paths(root.path());
        assert_eq!(
            paths.resolve(None).unwrap(),
            root.path().join("home/Pictures")
        );
        paths.pictures = Some("$HOME/图片".into());
        assert_eq!(paths.resolve(None).unwrap(), root.path().join("home/图片"));
        let config = paths.config.as_ref().unwrap();
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        fs::write(config, "save_dir = '~/截图 目录'\n").unwrap();
        assert_eq!(
            paths.resolve(None).unwrap(),
            root.path().join("home/截图 目录")
        );
        fs::write(config, "save_dir = 'relative-shots'\n").unwrap();
        assert_eq!(
            paths.resolve(None).unwrap(),
            root.path().join("config/babry/relative-shots")
        );
        paths.env_save_dir = Some("${HOME}/env".into());
        assert_eq!(paths.resolve(None).unwrap(), root.path().join("home/env"));
        assert_eq!(
            paths.resolve(Some(Path::new("cli-shots"))).unwrap(),
            root.path().join("working/cli-shots")
        );
        assert!(
            !root.path().join("working/cli-shots").exists(),
            "resolving a path created a directory before export"
        );
    }

    #[test]
    fn malformed_config_or_invalid_directory_reports_an_error_but_cli_can_override() {
        let root = tempfile::tempdir().unwrap();
        let paths = paths(root.path());
        let config = paths.config.as_ref().unwrap();
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        for content in ["save_dir = 42", "save_dir = ''", "save_dir = ["] {
            fs::write(config, content).unwrap();
            assert!(
                paths.resolve(None).is_err(),
                "accepted invalid config: {content}"
            );
            assert!(paths.resolve(Some(root.path())).is_ok());
        }
        let file = root.path().join("existing-file");
        fs::write(&file, "not a directory").unwrap();
        assert!(paths.resolve(Some(&file)).is_err());
        assert!(output_path(&file).is_err());
        assert!(paths.resolve(Some(Path::new(""))).is_err());
        assert!(expand_path(Path::new("~/shots"), None, root.path()).is_err());
    }

    #[test]
    fn exports_create_missing_directories_and_keep_png_extension() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("new/截图");
        let output = output_path(&directory).unwrap();
        assert!(directory.is_dir());
        assert_eq!(output.parent(), Some(directory.as_path()));
        assert_eq!(output.extension().unwrap(), "png");
    }
}
