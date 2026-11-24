//! Internationalization (English and Turkish) with live runtime switching.
//!
//! Static UI text lives in Slint's `Tr` global and is updated in one pass by
//! [`apply_to_ui`] whenever the user changes the language in Settings. Dynamic
//! strings formatted in Rust use [`pick`] and the localized date/event helpers
//! below.

use std::rc::Rc;
use std::sync::atomic::{AtomicU8, Ordering};

use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use crate::config::Language;
use crate::{Bridge, MainWindow, Tr};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    En,
    Tr,
}

static CURRENT: AtomicU8 = AtomicU8::new(0);

/// Detects the OS UI language (Turkish -> `Lang::Tr`, everything else -> `Lang::En`).
pub fn detect_system() -> Lang {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Globalization::GetUserDefaultUILanguage;
        // Primary language identifier (low 10 bits): 0x1F = LANG_TURKISH.
        let lang_id = unsafe { GetUserDefaultUILanguage() };
        if (lang_id & 0x03FF) == 0x001F {
            return Lang::Tr;
        }
        return Lang::En;
    }
    #[cfg(not(windows))]
    {
        for key in ["LC_ALL", "LC_MESSAGES", "LANG"] {
            if let Ok(val) = std::env::var(key) {
                let lower = val.to_ascii_lowercase();
                if lower.starts_with("tr") {
                    return Lang::Tr;
                }
                if !lower.is_empty() && lower != "c" && lower != "posix" {
                    return Lang::En;
                }
            }
        }
        Lang::En
    }
}

pub fn resolve(pref: Language) -> Lang {
    match pref {
        Language::System => detect_system(),
        Language::English => Lang::En,
        Language::Turkish => Lang::Tr,
    }
}

pub fn set(pref: Language) -> Lang {
    let lang = resolve(pref);
    CURRENT.store(if lang == Lang::Tr { 1 } else { 0 }, Ordering::Relaxed);
    lang
}

pub fn current() -> Lang {
    if CURRENT.load(Ordering::Relaxed) == 1 { Lang::Tr } else { Lang::En }
}

pub fn is_tr() -> bool {
    current() == Lang::Tr
}

pub fn pick<'a>(en: &'a str, tr: &'a str) -> &'a str {
    if is_tr() { tr } else { en }
}

pub fn month_name(month: u32) -> &'static str {
    if is_tr() {
        match month {
            1 => "Ocak",
            2 => "Şubat",
            3 => "Mart",
            4 => "Nisan",
            5 => "Mayıs",
            6 => "Haziran",
            7 => "Temmuz",
            8 => "Ağustos",
            9 => "Eylül",
            10 => "Ekim",
            11 => "Kasım",
            12 => "Aralık",
            _ => "",
        }
    } else {
        match month {
            1 => "January",
            2 => "February",
            3 => "March",
            4 => "April",
            5 => "May",
            6 => "June",
            7 => "July",
            8 => "August",
            9 => "September",
            10 => "October",
            11 => "November",
            12 => "December",
            _ => "",
        }
    }
}

pub fn short_month(month: u32) -> &'static str {
    if is_tr() {
        match month {
            1 => "Oca",
            2 => "Şub",
            3 => "Mar",
            4 => "Nis",
            5 => "May",
            6 => "Haz",
            7 => "Tem",
            8 => "Ağu",
            9 => "Eyl",
            10 => "Eki",
            11 => "Kas",
            12 => "Ara",
            _ => "",
        }
    } else {
        match month {
            1 => "Jan",
            2 => "Feb",
            3 => "Mar",
            4 => "Apr",
            5 => "May",
            6 => "Jun",
            7 => "Jul",
            8 => "Aug",
            9 => "Sep",
            10 => "Oct",
            11 => "Nov",
            12 => "Dec",
            _ => "",
        }
    }
}

pub fn detection_label(code: i64) -> String {
    if !is_tr() {
        return crate::cloud::clip_kind(code);
    }
    match code {
        1 => "Sürekli kayıt".into(),
        2 => "Hareket".into(),
        3 => "Kamera müdahalesi".into(),
        4 => "Çizgi ihlali".into(),
        5 => "Alan ihlali".into(),
        6 => "Kişi".into(),
        7 => "Bebek ağlaması".into(),
        8 => "Araç".into(),
        9 => "Evcil hayvan".into(),
        10 => "Kapı zili".into(),
        11 => "Köpek havlaması".into(),
        12 => "Kedi miyavlaması".into(),
        13 => "Cam kırılması".into(),
        14 => "Duman alarmı".into(),
        15 => "Paket teslimi".into(),
        16 => "Paket alındı".into(),
        17 => "Cevapsız zil".into(),
        18 => "Yanıtlanan zil".into(),
        19 => "Hırsızlık önleme".into(),
        20 => "Yüz".into(),
        21 => "Tanıdık olmayan yüz".into(),
        22 => "Tanıdık olmayan kişi".into(),
        23..=26 | 30 => "Bebek monitörü".into(),
        27 => "Yüz kapalı".into(),
        28 => "Güvenli alan dışı".into(),
        other => format!("Olay {other}"),
    }
}

pub fn disk_label(raw: &str) -> String {
    match raw {
        "HDD" | "Hard disk" | "Hard Disk" => pick("Hard disk", "Sabit disk").to_string(),
        "Internal" | "Built-in storage" | "Built-in Storage" => {
            pick("Built-in storage", "Dahili depolama").to_string()
        }
        "SD card" | "SD Card" => pick("SD card", "SD kart").to_string(),
        "USB drive" | "USB Drive" => pick("USB drive", "USB sürücü").to_string(),
        other => other.to_string(),
    }
}

fn model(items: &[&str]) -> ModelRc<SharedString> {
    ModelRc::from(Rc::new(VecModel::from(
        items.iter().map(|&s| SharedString::from(s)).collect::<Vec<_>>(),
    )))
}

/// Applies all static labels and dropdown models for `lang` to the Slint UI.
pub fn apply_to_ui(ui: &MainWindow, lang: Lang) {
    let tr = ui.global::<Tr>();
    let b = ui.global::<Bridge>();
    let t = |en: &str, tr_s: &str| SharedString::from(if lang == Lang::Tr { tr_s } else { en });

    // Navigation & window controls
    tr.set_nav_home(t("Home", "Ana sayfa"));
    tr.set_nav_live(t("Live view", "Canlı izleme"));
    tr.set_nav_playback(t("Playback", "Kayıtlar"));
    tr.set_nav_people(t("People", "Yüzler"));
    tr.set_nav_settings(t("Settings", "Ayarlar"));
    tr.set_win_minimize(t("Minimize", "Simge durumuna küçült"));
    tr.set_win_maximize(t("Maximize", "Ekranı kapla"));
    tr.set_win_restore(t("Restore", "Önceki boyut"));
    tr.set_win_close(t("Close", "Kapat"));

    // Sign-in & welcome screens
    tr.set_welcome_tagline(t(
        "Your Tapo cameras on your desktop, from anywhere.",
        "Tapo kameralarınız masaüstünüzde, her yerden erişilebilir.",
    ));
    tr.set_log_in(t("Log In", "Giriş Yap"));
    tr.set_logging_in(t("Logging in…", "Giriş yapılıyor…"));
    tr.set_create_id(t("Create a TP-Link ID", "TP-Link Kimliği Oluştur"));
    tr.set_sign_up(t("Sign Up", "Kaydol"));
    tr.set_forgot_password(t("Forgot Password?", "Şifremi Unuttum?"));
    tr.set_remember_me(t("Remember me", "Beni hatırla"));
    tr.set_login_subtitle(t(
        "Use your TP-Link ID: the email and password you use in the Tapo app.",
        "TP-Link Kimliğinizi kullanın: Tapo uygulamasındaki e-posta ve şifreniz.",
    ));
    tr.set_email_placeholder(t("TP-Link ID (email)", "TP-Link Kimliği (e-posta)"));
    tr.set_password_placeholder(t("Password", "Şifre"));
    tr.set_show_password(t("Show password", "Şifreyi göster"));
    tr.set_hide_password(t("Hide password", "Şifreyi gizle"));
    tr.set_sign_up_help(t(
        "TP-Link IDs are created in the Tapo app: open it on your phone, tap “Create a TP-Link ID” and follow the steps. Then log in here with the same email and password.",
        "TP-Link Kimlikleri Tapo uygulamasından oluşturulur: telefonunuzda uygulamayı açıp “TP-Link Kimliği Oluştur” adımlarını izleyin, ardından aynı bilgilerle buraya giriş yapın.",
    ));
    tr.set_forgot_help(t(
        "Reset your password in the Tapo app: on its login screen, tap “Forgot Password?”. Then log in here with the new password.",
        "Şifrenizi Tapo uygulamasının giriş ekranındaki “Şifremi Unuttum?” seçeneğiyle sıfırlayın, ardından yeni şifrenizle buraya giriş yapın.",
    ));
    tr.set_mfa_title(t("Two-Step Verification", "İki Adımlı Doğrulama"));
    tr.set_mfa_resend(t("Resend Code", "Kodu Tekrar Gönder"));
    tr.set_mfa_verify(t("Verify", "Doğrula"));

    // Home page & hub card
    tr.set_update_camera_list(t("Update camera list", "Kamera listesini güncelle"));
    tr.set_update_available_suffix(t(" is available.", " sürümü mevcut."));
    tr.set_update_now(t("Update now", "Şimdi güncelle"));
    tr.set_later(t("Later", "Sonra"));
    tr.set_hub_section(t("Hub", "Hub"));
    tr.set_cameras_section(t("Cameras", "Kameralar"));
    tr.set_no_cameras_home(t(
        "No cameras yet. The cameras on your Tapo account are added when you sign in; press ⟳ to look again.",
        "Henüz kamera yok. Tapo hesabınızdaki kameralar giriş yapınca otomatik eklenir; tekrar kontrol etmek için ⟳ tuşuna basın.",
    ));
    tr.set_loop_recording(t("Loop recording", "Döngüsel kayıt"));
    tr.set_ring_hub(t("Ring hub", "Hub alarmını çal"));
    tr.set_stop_alarm(t("Stop alarm", "Alarmı durdur"));
    tr.set_asking_storage(t("Asking the hub about its storage…", "Hub depolama bilgisi alınıyor…"));

    // Live view & single camera view
    tr.set_fullscreen(t("Fullscreen", "Tam ekran"));
    tr.set_fullscreen_hint(t("Fullscreen (F11)", "Tam ekran (F11)"));
    tr.set_leave_fullscreen(t("Leave full screen", "Tam ekrandan çık"));
    tr.set_no_cameras_title(t("No cameras yet", "Henüz kamera yok"));
    tr.set_no_cameras_live(t(
        "The cameras on your Tapo account appear here once they've been listed: press ⟳ on Home to look again.",
        "Tapo hesabınızdaki kameralar listelendiğinde burada görünür: tekrar bakmak için Ana sayfadaki ⟳ tuşuna basın.",
    ));
    tr.set_back(t("Back", "Geri"));
    tr.set_mute_audio(t("Mute audio (M)", "Sesi kapat (M)"));
    tr.set_listen_in(t("Listen in (M)", "Sesi aç (M)"));
    tr.set_sound_on(t("Sound On", "Ses Açık"));
    tr.set_sound_off(t("Sound Off", "Ses Kapalı"));
    tr.set_take_photo(t("Take Photo", "Fotoğraf Çek"));
    tr.set_record_live(t("Record Video", "Video Kaydet"));
    tr.set_stop_recording(t("Stop Rec", "Kaydı Durdur"));
    tr.set_reset_zoom(t("Reset zoom (0)", "Yakınlaştırmayı sıfırla (0)"));
    tr.set_pan_tilt(t("Pan & Tilt", "Yönlendirme"));
    tr.set_privacy_mode(t("Privacy Mode", "Gizlilik Modu"));
    tr.set_privacy_on_title(t("Privacy mode is on", "Gizlilik modu açık"));
    tr.set_privacy_on_desc(t(
        "The camera has covered its lens, so there's no video.",
        "Kamera lensini kapattığı için görüntü yok.",
    ));
    tr.set_turn_off(t("Turn off", "Kapat"));
    tr.set_spotlight(t("Spotlight", "Spot Işığı"));
    tr.set_spotlight_on(t("Spotlight On", "Spot Açık"));
    tr.set_auto_track(t("Auto-Track", "Oto. Takip"));
    tr.set_tracking_on(t("Tracking On", "Takip Açık"));
    tr.set_siren(t("Siren", "Siren"));
    tr.set_stop_siren(t("Stop Siren", "Sireni Durdur"));
    tr.set_night_vision(t("Night Vision", "Gece Görüşü"));
    tr.set_playback_download(t("Playback & Download", "Kayıtlar ve İndirme"));
    tr.set_reboot_camera(t("Reboot camera", "Kamerayı yeniden başlat"));
    tr.set_reboot_confirm_prefix(t("Reboot ", ""));
    tr.set_reboot_confirm_suffix(t(
        "? Video will pause for ~30 seconds.",
        " yeniden başlatılsın mı? Görüntü ~30 saniye kesilir.",
    ));
    tr.set_confirm_reboot(t("Confirm Reboot", "Yeniden Başlat"));
    tr.set_cancel(t("Cancel", "İptal"));
    tr.set_save(t("Save", "Kaydet"));
    tr.set_step_hint_prefix(t("Step ", "Adım "));
    tr.set_step_hint_suffix(t("  ·  arrow keys work too", "  ·  ok tuşları da çalışır"));
    tr.set_pan_patrol(t("Pan patrol", "Yatay tarama"));
    tr.set_tilt_patrol(t("Tilt patrol", "Dikey tarama"));
    tr.set_stop(t("Stop", "Durdur"));
    tr.set_presets(t("Presets", "Kayıtlı Konumlar"));
    tr.set_refresh(t("Refresh", "Yenile"));
    tr.set_no_presets(t(
        "No presets yet: move the camera, name the position and save it.",
        "Henüz kayıtlı konum yok: kamerayı yönlendirin, bir ad verin ve kaydedin.",
    ));
    tr.set_preset_placeholder(t("Name this position", "Bu konuma ad verin"));

    // Playback page
    tr.set_show_list(t("Show the list", "Listeyi göster"));
    tr.set_hide_list(t("Hide the list", "Listeyi gizle"));
    tr.set_reload(t("Reload", "Yeniden yükle"));
    tr.set_no_recordings_day(t("No recordings on this day.", "Bu güne ait kayıt yok."));
    tr.set_click_timeline_hint(t(
        "Click the timeline, or a recording on the right, to watch from there.",
        "İzlemek için zaman çizelgesine veya sağdaki bir kayda tıklayın.",
    ));
    tr.set_play(t("Play", "Oynat"));
    tr.set_pause(t("Pause", "Duraklat"));
    tr.set_record(t("Record", "Kaydet"));
    tr.set_stop_save_recording(t("Stop and save the recording", "Kaydı durdur ve kaydet"));
    tr.set_close_player(t("Close the player", "Oynatıcıyı kapat"));
    tr.set_zoom_in(t("Zoom in", "Yakınlaştır"));
    tr.set_zoom_out(t("Zoom out", "Uzaklaştır"));
    tr.set_saved_to_videos(t("Saved to Videos", "Videolar'a kaydedilenler"));
    tr.set_open(t("Open", "Aç"));
    tr.set_show_in_folder(t("Show in folder", "Klasörde göster"));
    tr.set_today(t("Today", "Bugün"));
    tr.set_prev_month(t("Previous month", "Önceki ay"));
    tr.set_next_month(t("Next month", "Sonraki ay"));
    tr.set_saved_tag(t("saved", "kaydedildi"));
    tr.set_saved_tooltip(t("Saved", "Kaydedildi"));
    tr.set_save_to_videos(t("Save to Videos", "Videolar'a kaydet"));
    tr.set_weekdays(if lang == Lang::Tr {
        model(&["Pt", "Sa", "Ça", "Pe", "Cu", "Ct", "Pz"])
    } else {
        model(&["Mo", "Tu", "We", "Th", "Fr", "Sa", "Su"])
    });
    tr.set_activity_24h(t("24h activity", "24 saatlik hareketlilik"));
    tr.set_batch_select(t("Select", "Seç"));
    tr.set_batch_done(t("Done", "Bitti"));
    tr.set_batch_all(t("All", "Tümü"));
    tr.set_batch_clear(t("Clear", "Temizle"));
    tr.set_batch_export_prefix(t("Export ", ""));
    tr.set_batch_export_suffix(t(" MP4", " MP4 Dışa Aktar"));

    // People / H500 facial recognition page
    tr.set_people_subtitle(t(
        "Recognized faces and cross-camera tracking on your H500 HomeBase.",
        "H500 HomeBase üzerindeki tanınan yüzler ve kameralar arası takip.",
    ));
    tr.set_people_all_option(t("All people", "Tüm kişiler"));
    tr.set_people_familiar(t("Familiar", "Tanıdık"));
    tr.set_people_stranger(t("Stranger", "Yabancı"));
    tr.set_people_edit_title(t("Edit person", "Kişiyi düzenle"));
    tr.set_people_name_placeholder(t("Enter name", "Ad girin"));
    tr.set_people_mark_familiar(t("Mark as familiar", "Tanıdık olarak işaretle"));
    tr.set_people_timeline_title(t("Cross-camera timeline", "Kameralar arası zaman çizelgesi"));
    tr.set_people_prev_day(t("Previous day", "Önceki gün"));
    tr.set_people_next_day(t("Next day", "Sonraki gün"));
    tr.set_people_watch(t("Watch", "İzle"));

    // Settings page
    tr.set_sec_account(t("Tapo account", "Tapo hesabı"));
    tr.set_not_signed_in(t("Not signed in", "Giriş yapılmadı"));
    tr.set_signed_in_prefix(t("Signed in to the Tapo cloud · ", "Tapo bulutuna giriş yapıldı · "));
    tr.set_sign_out(t("Sign out", "Çıkış yap"));
    tr.set_cameras_and_hubs(t("Cameras and hubs", "Kameralar ve hub'lar"));
    tr.set_cameras_hubs_desc(t(
        "Added by themselves; update after adding one in the Tapo app.",
        "Otomatik eklenir; Tapo uygulamasında yeni cihaz ekledikten sonra güncelleyin.",
    ));
    tr.set_update_list(t("Update list", "Listeyi güncelle"));
    tr.set_cloud_note(t(
        "Live video, pan/tilt and the hub's recordings go through TP-Link's cloud, as in the Tapo app, so this PC needn't be on the cameras' network. It's TP-Link's private service, which they can change.",
        "Canlı görüntü, yönlendirme ve hub kayıtları Tapo uygulamasındaki gibi TP-Link bulutu üzerinden çalışır; bu bilgisayarın kameralarla aynı ağda olması gerekmez.",
    ));
    tr.set_no_cameras_settings_desc(t(
        "Sign in above: the cameras on your Tapo account are added by themselves.",
        "Yukarıdan giriş yapın: Tapo hesabınızdaki kameralar otomatik olarak eklenir.",
    ));
    tr.set_edit(t("Edit", "Düzenle"));
    tr.set_add_camera(t("Add a camera", "Kamera ekle"));
    tr.set_edit_camera(t("Edit camera", "Kamerayı düzenle"));
    tr.set_tapo_camera(t("Tapo camera", "Tapo kamerası"));
    tr.set_name_label(t("Name", "Ad"));
    tr.set_name_placeholder(t("Same as in the Tapo app", "Tapo uygulamasındakiyle aynı"));
    tr.set_live_stream_label(t("Live view stream", "Canlı izleme akışı"));
    tr.set_stream_quality_note(t(
        "SD is light on the network and the PC, for the grid; HD is the camera's full picture. A camera opened on its own is always HD to start with.",
        "SD, çoklu görünüm için ağı ve bilgisayarı az yorar; HD kameranın tam çözünürlüğüdür. Tek kamera görünümü her zaman HD açılır.",
    ));
    tr.set_add_camera_desc(t(
        "Cameras removed here, or ones the list missed, can be added back.",
        "Buradan kaldırılan veya listede eksik kalan kameralar tekrar eklenebilir.",
    ));
    tr.set_add(t("Add", "Ekle"));
    tr.set_sec_appearance(t("General", "Genel"));
    tr.set_appearance_title(t("Appearance", "Görünüm"));
    tr.set_appearance_desc(t("Light, dark, or the same as Windows.", "Açık, koyu veya Windows ile aynı."));
    tr.set_language_title(t("Language", "Dil"));
    tr.set_language_desc(t(
        "Display language; applies immediately.",
        "Uygulama dili; anında uygulanır.",
    ));
    tr.set_renderer_title(t("Graphics renderer", "Grafik motoru"));
    tr.set_renderer_desc(t(
        "How the window is drawn, from the next start.",
        "Pencere çizim motoru (sonraki açılışta geçerli olur).",
    ));
    tr.set_settings_file(t("Settings file", "Ayar dosyası"));
    tr.set_sec_shortcuts(t("Keyboard shortcuts", "Klavye kısayolları"));
    tr.set_sc_nav(t(
        "Switch pages (Home, Live view, Playback, People, Settings)",
        "Sayfalar arası geçiş (Ana sayfa, Canlı izleme, Kayıtlar, Yüzler, Ayarlar)",
    ));
    tr.set_sc_cam(t(
        "Open camera 1–9 from Home or Live view",
        "Ana sayfa veya Canlı izlemeden 1–9 numaralı kamerayı aç",
    ));
    tr.set_sc_space(t("Play or pause in Playback", "Kayıtlarda oynat veya duraklat"));
    tr.set_sc_arrows(t(
        "Skip ±10s in Playback (Shift: ±30s), or pan PTZ camera",
        "Kayıtlarda ±10 sn atla (Shift: ±30 sn) veya PTZ kamerayı yönlendir",
    ));
    tr.set_sc_speed(t("Decrease or increase Playback speed", "Kayıt oynatma hızını azalt veya artır"));
    tr.set_sc_mute(t("Mute or unmute audio", "Sesi aç veya kapat"));
    tr.set_sc_snap(t("Take snapshot (Pictures\\Tapas)", "Fotoğraf çek (Resimler\\Tapas)"));
    tr.set_sc_rec(t(
        "Start or stop MP4 recording (Videos\\Tapas)",
        "MP4 kaydını başlat veya durdur (Videolar\\Tapas)",
    ));
    tr.set_sc_zoom(t(
        "Scroll to zoom (1×–8×), drag to pan; press 0 to reset",
        "Yakınlaştırmak için kaydır (1×–8×), sürükleyerek gez; sıfırlamak için 0",
    ));
    tr.set_sc_full(t(
        "Toggle or leave fullscreen, or go back from Single view",
        "Tam ekranı aç/kapat veya tek kamera görünümünden geri dön",
    ));
    tr.set_sec_about(t("About and updates", "Hakkında ve güncellemeler"));
    tr.set_about_default(t(
        "TP-Link Tapo cameras and the H500 HomeBase, through the Tapo cloud.",
        "Tapo bulutu üzerinden TP-Link Tapo kameralar ve H500 HomeBase.",
    ));
    tr.set_update_to_prefix(t("Update to ", "Güncelle: "));
    tr.set_check_updates(t("Check for updates", "Güncellemeleri denetle"));
    tr.set_auto_update_title(t("Check for updates at start-up", "Açılışta güncellemeleri denetle"));
    tr.set_auto_update_desc(t(
        "When a new version is available, it installs automatically and Tapas restarts.",
        "Yeni sürüm çıktığında otomatik olarak kurulur ve Tapas yeniden başlar.",
    ));
    tr.set_credits_note(t(
        "Uses FFmpeg (LGPL) and open-source Tapo protocol libraries (MIT).",
        "FFmpeg (LGPL) ve açık kaynaklı Tapo protokol kütüphanelerini (MIT) kullanır.",
    ));

    // Localized ComboBox & filter chip models
    tr.set_appearance_labels(if lang == Lang::Tr {
        model(&["Windows ile aynı", "Açık", "Koyu"])
    } else {
        model(&["Same as Windows", "Light", "Dark"])
    });
    tr.set_language_labels(if lang == Lang::Tr {
        model(&["Sistem (Otomatik)", "English", "Türkçe"])
    } else {
        model(&["System (Auto)", "English", "Türkçe"])
    });
    tr.set_renderer_labels(if lang == Lang::Tr {
        model(&["OpenGL (önerilen)", "Yazılım (CPU)"])
    } else {
        model(&["OpenGL (recommended)", "Software (CPU)"])
    });

    b.set_nv_labels(if lang == Lang::Tr {
        model(&["Otomatik", "Gündüz", "Kızılötesi", "Renkli", "Akıllı"])
    } else {
        model(&["Auto", "Day", "Infrared", "Color", "Smart"])
    });
    b.set_rec_filter_labels(if lang == Lang::Tr {
        model(&["Tümü", "Kişi", "Araç", "Evcil", "Hareket", "Uyarılar"])
    } else {
        model(&["All", "Person", "Vehicle", "Pet", "Motion", "Alerts"])
    });
    b.set_face_filter_labels(if lang == Lang::Tr {
        model(&["Tümü", "Tanıdık", "Yabancı"])
    } else {
        model(&["All", "Familiar", "Stranger"])
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_explicit_language_preferences() {
        assert_eq!(resolve(Language::English), Lang::En);
        assert_eq!(resolve(Language::Turkish), Lang::Tr);
        assert_eq!(Language::from_index(Language::System.index()), Language::System);
        assert_eq!(Language::from_index(Language::English.index()), Language::English);
        assert_eq!(Language::from_index(Language::Turkish.index()), Language::Turkish);
    }

    #[test]
    fn translates_months_and_detections() {
        set(Language::Turkish);
        assert_eq!(month_name(11), "Kasım");
        assert_eq!(short_month(2), "Şub");
        assert_eq!(detection_label(6), "Kişi");
        assert_eq!(detection_label(8), "Araç");
        assert_eq!(disk_label("Hard disk"), "Sabit disk");

        set(Language::English);
        assert_eq!(month_name(11), "November");
        assert_eq!(short_month(2), "Feb");
        assert_eq!(detection_label(6), "Person");
        assert_eq!(detection_label(8), "Vehicle");
        assert_eq!(disk_label("Hard disk"), "Hard disk");
    }
}
