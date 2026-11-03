#[test]
#[ignore = "needs an interactive test Wayland compositor; sends Escape and starts a private daemon"]
fn daemon_repeated_captures_cancel_and_recover_from_invalid_output() {
    let (_temp, dir) = directory("daemon");
    let output = compositor::select_output(&compositor::outputs().unwrap(), None).unwrap();
    let input = Input::new((output.width, output.height));
    // 使用同一 Wayland socket 的绝对路径产生独立 IPC 名称，不接管桌面已有后台。
    let display = std::env::var_os("WAYLAND_DISPLAY").unwrap();
    let display = PathBuf::from(display);
    let display = if display.is_absolute() {
        display
    } else {
        PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").unwrap()).join(display)
    };
    let command = || {
        let mut c = Command::new(binary());
        c.env("WAYLAND_DISPLAY", &display);
        c
    };
    let log = File::create(dir.join("daemon.log")).unwrap();
    let mut daemon = Editor(
        command()
            .arg("--daemon")
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .unwrap(),
    );
    pause(200);
    for cycle in 0..3 {
        let response = command()
            .args(["--screen", "--save-dir"])
            .arg(dir.join("export"))
            .output()
            .unwrap();
        assert!(response.status.success(), "capture {cycle} request failed");
        pause(400);
        let status = command().arg("--daemon-status").output().unwrap();
        assert!(String::from_utf8_lossy(&status.stdout).contains("正在处理"));
        let busy = command().arg("--screen").output().unwrap();
        assert!(
            !busy.status.success(),
            "concurrent capture should be rejected"
        );
        input.key(1, "Escape", false);
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let status = command().arg("--daemon-status").output().unwrap();
            if String::from_utf8_lossy(&status.stdout).contains("空闲") {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "daemon did not become idle after Escape"
            );
            pause(40);
        }
        pause(120);
    }
    let _ = command()
        .args(["--output", "babry-nonexistent-output"])
        .output()
        .unwrap();
    pause(200);
    let status = command().arg("--daemon-status").output().unwrap();
    assert!(String::from_utf8_lossy(&status.stdout).contains("空闲"));
    assert!(command().arg("--stop-daemon").status().unwrap().success());
    daemon.wait();
    println!(
        "daemon capture/Escape/busy/error recovery passed on {:?}",
        Backend::detect().unwrap()
    );
}
