use super::*;

fn window() -> AppWindow {
    slint::platform::set_platform(Box::new(crate::app::tests::SoftwareTestPlatform)).ok();

    AppWindow::new().unwrap()
}

#[test]
fn missing_encoder_can_be_cancelled_without_changing_the_recording_preference() {
    let window = window();

    window.set_recording_mp4(true);
    wire(&window, false);
    show_missing(&window);
    assert!(window.get_ffmpeg_setup_visible());
    assert!(window.get_busy());
    assert!(window.get_ffmpeg_setup_message().contains("Download"));
    window.invoke_cancel_ffmpeg_setup();
    assert!(!window.get_ffmpeg_setup_visible());
    assert!(!window.get_busy());
    assert!(window.get_recording_mp4());
}

#[test]
fn cancelling_an_active_download_waits_for_worker_cleanup() {
    let window = window();

    wire(&window, false);
    show_missing(&window);
    window.set_ffmpeg_downloading(true);
    window.invoke_cancel_ffmpeg_setup();
    assert!(window.get_busy());
    assert!(window.get_ffmpeg_setup_visible());
    finish(&window, true, Err(anyhow::anyhow!("cancelled")));
    assert!(!window.get_ffmpeg_downloading());
    assert!(!window.get_busy());
    assert!(!window.get_ffmpeg_setup_visible());
}

#[test]
fn success_dismisses_setup_and_failure_keeps_retry_available() {
    let window = window();

    show_missing(&window);
    window.set_ffmpeg_downloading(true);
    finish(&window, false, Err(anyhow::anyhow!("checksum mismatch")));
    assert!(window.get_ffmpeg_setup_visible());
    assert!(window.get_busy());
    assert!(!window.get_ffmpeg_downloading());
    assert!(
        window
            .get_ffmpeg_setup_message()
            .contains("checksum mismatch")
    );
    finish(&window, false, Ok(PathBuf::from("ffmpeg")));
    assert!(!window.get_ffmpeg_setup_visible());
    assert!(!window.get_busy());
    assert_eq!(window.get_status_text(), "FFmpeg ready");
    assert!(!window.get_recording());
}

#[test]
fn keyboard_navigation_stays_in_the_setup_dialog() {
    use slint::platform::{Key, WindowEvent};

    let window = window();
    let downloads = std::rc::Rc::new(std::cell::Cell::new(0));
    let downloads_callback = std::rc::Rc::clone(&downloads);
    let cancellations = std::rc::Rc::new(std::cell::Cell::new(0));
    let cancellations_callback = std::rc::Rc::clone(&cancellations);

    window.on_download_ffmpeg(move || downloads_callback.set(downloads_callback.get() + 1));
    window.on_cancel_ffmpeg_setup(move || {
        cancellations_callback.set(cancellations_callback.get() + 1);
    });
    show_missing(&window);
    let press = |key: Key| {
        window
            .window()
            .dispatch_event(WindowEvent::KeyPressed { text: key.into() });
        window
            .window()
            .dispatch_event(WindowEvent::KeyReleased { text: key.into() });
    };

    press(Key::Return);
    assert_eq!(downloads.get(), 1);

    for _ in 0..5 {
        press(Key::Tab);
    }

    press(Key::Return);
    assert_eq!(cancellations.get(), 1);
    press(Key::Escape);
    assert_eq!(cancellations.get(), 2);
    window.set_ffmpeg_downloading(true);
    press(Key::Tab);
    press(Key::Return);
    assert_eq!(downloads.get(), 1);
    assert_eq!(cancellations.get(), 3);
}
