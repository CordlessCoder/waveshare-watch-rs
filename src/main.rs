#![no_std]
#![no_main]

extern crate alloc;

mod board;
mod drivers;
mod peripherals;
mod ui;
mod apps;

use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;

use embedded_graphics_core::prelude::RawData;
use embedded_hal_bus::i2c::RefCellDevice;
use esp_alloc as _;
use esp_backtrace as _;

esp_bootloader_esp_idf::esp_app_desc!();

use embassy_executor::Spawner;
use embassy_time::{Duration, Instant, Timer};

use esp_hal::delay::Delay;
use esp_hal::dma::{DmaRxBuf, DmaTxBuf};
use esp_hal::dma_buffers;
// use esp_hal::i2s::master::{I2s, Config as I2sConfig, DataFormat}; // TODO: wire I2S
use esp_hal::gpio::{InputConfig, Level, Output, OutputConfig, Pull, Input};
use esp_hal::i2c::master::{Config as I2cConfig, I2c};
use esp_hal::interrupt::software::SoftwareInterruptControl;
use esp_hal::spi::master::{Config as SpiConfig, Spi};
use esp_hal::spi::Mode as SpiMode;
use esp_hal::time::Rate;
use esp_hal::timer::timg::TimerGroup;
use esp_println::println;

use crate::drivers::co5300::Co5300Display;
use crate::drivers::framebuffer::Framebuffer;
use crate::drivers::qspi_bus::QspiBus;
use crate::peripherals::power::Axp2101Power;
use crate::peripherals::touch::{Ft3168Touch, SwipeDirection};
use crate::peripherals::rtc::{Pcf85063aRtc, DateTime};
use crate::peripherals::imu::Qmi8658Imu;
use crate::ui::watchface::WatchFace;
use crate::ui::pages::{self, Page};
use crate::apps::{App, AppInput, AppResult, AppState};
use crate::apps::snake::SnakeGame;
use crate::apps::game2048::Game2048;
use crate::apps::tetris::TetrisGame;
use crate::apps::flappy::FlappyGame;
use crate::apps::maze::MazeGame;
use crate::ui::launcher::Launcher;
use crate::apps::settings::SettingsApp;
use crate::apps::mp3player::Mp3Player;
use crate::apps::smarthome::SmartHomeApp;
use crate::peripherals::audio::{Es8311, fill_beep_buffer};

// Network runner task (must be spawned for WiFi to work)
#[embassy_executor::task]
async fn net_task(mut runner: embassy_net::Runner<'static, esp_radio::wifi::WifiDevice<'static>>) -> ! {
    runner.run().await
}

// Simple NTP sync (UDP to pool.ntp.org:123)
async fn ntp_sync(
    stack: embassy_net::Stack<'static>,
    rtc: &mut crate::peripherals::rtc::Pcf85063aRtc<impl embedded_hal::i2c::I2c>,
) -> Result<(), ()> {
    use embassy_net::udp::{UdpSocket, PacketMetadata};

    let mut rx_meta = [PacketMetadata::EMPTY; 1];
    let mut rx_buf = [0u8; 256];
    let mut tx_meta = [PacketMetadata::EMPTY; 1];
    let mut tx_buf = [0u8; 256];

    let mut socket = UdpSocket::new(stack, &mut rx_meta, &mut rx_buf, &mut tx_meta, &mut tx_buf);
    socket.bind(12345).map_err(|_| ())?;

    // NTP request packet (simplified: 48 bytes, first byte = 0x1B for client mode)
    let mut ntp_request = [0u8; 48];
    ntp_request[0] = 0x1B; // LI=0, VN=3, Mode=3 (client)

    // Resolve pool.ntp.org (use Google's NTP IP directly: 216.239.35.0)
    let ntp_addr = embassy_net::Ipv4Address::new(216, 239, 35, 0);
    socket.send_to(&ntp_request, (ntp_addr, 123)).await.map_err(|_| ())?;

    // Wait for response (timeout 5s)
    let mut response = [0u8; 48];
    match embassy_time::with_timeout(
        Duration::from_secs(5),
        socket.recv_from(&mut response),
    ).await {
        Ok(Ok((len, _addr))) if len >= 48 => {
            // Parse NTP timestamp (bytes 40-43 = seconds since 1900-01-01)
            let ntp_secs = u32::from_be_bytes([response[40], response[41], response[42], response[43]]);
            // Convert NTP epoch (1900) to Unix epoch (1970): subtract 70 years in seconds
            let unix_secs = ntp_secs.wrapping_sub(2_208_988_800);
            // Convert to hours/minutes/seconds (UTC+2 for France)
            let utc_offset = 2 * 3600; // CEST (summer time)
            let local_secs = unix_secs + utc_offset;
            let time_of_day = local_secs % 86400;
            let hours = (time_of_day / 3600) as u8;
            let minutes = ((time_of_day % 3600) / 60) as u8;
            let seconds = (time_of_day % 60) as u8;

            // Calculate date (simplified: days since epoch)
            let total_days = (local_secs / 86400) as i32;
            // Simple date from days since 1970-01-01
            let (year, month, day) = days_to_date(total_days);

            println!("[NTP] Time: {:02}:{:02}:{:02} {:02}/{:02}/{}", hours, minutes, seconds, day, month, year);

            // Set RTC
            let dt = crate::peripherals::rtc::DateTime::new(
                (year - 2000) as u8, month as u8, day as u8,
                hours, minutes, seconds,
            );
            let _ = rtc.set_time(&dt);
            Ok(())
        }
        _ => Err(()),
    }
}

fn days_to_date(days_since_epoch: i32) -> (u32, u32, u32) {
    // Simplified date calculation from days since 1970-01-01
    let mut y = 1970i32;
    let mut remaining = days_since_epoch;
    loop {
        let days_in_year = if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) { 366 } else { 365 };
        if remaining < days_in_year { break; }
        remaining -= days_in_year;
        y += 1;
    }
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let month_days = [31, if leap { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut m = 0;
    while m < 12 && remaining >= month_days[m] {
        remaining -= month_days[m];
        m += 1;
    }
    (y as u32, (m + 1) as u32, (remaining + 1) as u32)
}

use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::RgbColor;

#[esp_rtos::main]
async fn main(_spawner: Spawner) {
    // Heap: 64KB SRAM + PSRAM for large allocs
    esp_alloc::heap_allocator!(size: 64 * 1024);
    // Power-aware: default to 160MHz instead of 240MHz.
    // Saves ~30% CPU power without noticeable impact on UI/sensor work.
    // Game code can still trigger short bursts via DMA/peripherals at 80MHz QSPI which is unchanged.
    let peripherals = esp_hal::init(
        esp_hal::Config::default()
            .with_cpu_clock(esp_hal::clock::CpuClock::_160MHz)
    );

    // PSRAM
    esp_alloc::psram_allocator!(peripherals.PSRAM, esp_hal::psram);

    // Embassy timer
    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0);

    esp_println::logger::init_logger_from_env();
    println!("=== Waveshare Watch RS v0.4 (Embassy) ===");

    let delay = Delay::new();

    // === I2C Bus ===
    let i2c = I2c::new(
        peripherals.I2C0,
        I2cConfig::default().with_frequency(Rate::from_khz(400)),
    )
    .expect("I2C failed")
    .with_sda(peripherals.GPIO15)
    .with_scl(peripherals.GPIO14);
    let i2c_ref = RefCell::new(i2c);

    // === Power ===
    let mut power = Axp2101Power::new(RefCellDevice::new(&i2c_ref));
    let _ = power.init();
    println!("[POWER] OK");

    // === Display 80MHz DMA ===
    let spi_config = SpiConfig::default()
        .with_frequency(Rate::from_mhz(80))
        .with_mode(SpiMode::_0);
    let (rx_buf, rx_desc, tx_buf, tx_desc) = dma_buffers!(8000);
    let dma_rx = DmaRxBuf::new(rx_desc, rx_buf).unwrap();
    let dma_tx = DmaTxBuf::new(tx_desc, tx_buf).unwrap();
    let spi = Spi::new(peripherals.SPI2, spi_config)
        .expect("SPI failed")
        .with_sck(peripherals.GPIO11)
        .with_sio0(peripherals.GPIO4)
        .with_sio1(peripherals.GPIO5)
        .with_sio2(peripherals.GPIO6)
        .with_sio3(peripherals.GPIO7)
        .with_dma(peripherals.DMA_CH0)
        .with_buffers(dma_rx, dma_tx);
    let cs = Output::new(peripherals.GPIO12, Level::High, OutputConfig::default());
    let reset = Output::new(peripherals.GPIO8, Level::High, OutputConfig::default());
    let mut display = Co5300Display::new(QspiBus::new(spi, cs), reset);
    display.init();

    // Enable Tearing Effect output on CO5300 (TE pin = GPIO13)
    // Command 0x35 = TEARON, param 0x00 = VBlank only
    display.bus_mut().write_c8d8(0x35, 0x00);
    let te_pin = Input::new(peripherals.GPIO13, InputConfig::default());
    println!("[DISPLAY] OK (TE VSync enabled)");

    // === Framebuffer PSRAM ===
    let mut fb = Framebuffer::new();
    fb.clear_color(Rgb565::BLACK);
    fb.flush(&mut display);
    println!("[FB] OK");

    // === Touch ===
    let mut touch_rst = Output::new(peripherals.GPIO9, Level::High, OutputConfig::default());
    // GPIO38 is the FT3168 INT line: held high by pull-up, pulled low by the controller
    // when a finger is on the screen. We use it both for level checks and as an async wake source.
    let mut touch_int = Input::new(peripherals.GPIO38, InputConfig::default().with_pull(Pull::Up));
    touch_rst.set_low(); delay.delay_millis(10); touch_rst.set_high(); delay.delay_millis(50);
    let mut touch = Ft3168Touch::new(RefCellDevice::new(&i2c_ref));
    let _ = touch.init();
    println!("[TOUCH] OK");

    // === RTC ===
    let mut rtc = Pcf85063aRtc::new(RefCellDevice::new(&i2c_ref));
    let _ = rtc.init();
    println!("[RTC] OK");

    // === IMU ===
    let mut imu = Qmi8658Imu::new(RefCellDevice::new(&i2c_ref));
    let _ = imu.init();
    println!("[IMU] OK");

    // === SD Card (SPI3) ===
    println!("[SD] Init...");
    let sd_spi_config = SpiConfig::default()
        .with_frequency(Rate::from_mhz(4))
        .with_mode(SpiMode::_0);
    let sd_spi = Spi::new(peripherals.SPI3, sd_spi_config)
        .expect("SPI3 failed")
        .with_sck(peripherals.GPIO2)
        .with_mosi(peripherals.GPIO1)
        .with_miso(peripherals.GPIO3);
    let sd_cs = Output::new(peripherals.GPIO17, Level::High, OutputConfig::default());

    use embedded_hal_bus::spi::ExclusiveDevice;
    let sd_spi_dev = ExclusiveDevice::new_no_delay(sd_spi, sd_cs).unwrap();
    let mut sd_card = embedded_sdmmc::SdCard::new(sd_spi_dev, delay);
    let mut sd_ok = false;
    let mut mp3_files: alloc::vec::Vec<alloc::string::String> = alloc::vec::Vec::new();
    match sd_card.num_bytes() {
        Ok(size) => {
            println!("[SD] Card {}MB", size / 1024 / 1024);

            // Scan /mp3/ directory
            struct DummyTime;
            impl embedded_sdmmc::TimeSource for DummyTime {
                fn get_timestamp(&self) -> embedded_sdmmc::Timestamp {
                    embedded_sdmmc::Timestamp::from_calendar(2026, 4, 6, 12, 0, 0).unwrap()
                }
            }

            let mut volume_mgr = embedded_sdmmc::VolumeManager::new(sd_card, DummyTime);
            match volume_mgr.open_raw_volume(embedded_sdmmc::VolumeIdx(0)) {
            Ok(volume) => {
                if let Ok(root_dir) = volume_mgr.open_root_dir(volume) {
                    if let Ok(mp3_dir) = volume_mgr.open_dir(root_dir, "MP3") {
                        println!("[SD] Found /MP3/ folder");
                        let _ = volume_mgr.iterate_dir(mp3_dir, |entry| {
                            if !entry.attributes.is_directory() {
                                let name = core::str::from_utf8(&entry.name.base_name()).unwrap_or("?");
                                let ext = core::str::from_utf8(&entry.name.extension()).unwrap_or("");
                                let full = alloc::format!("{}.{}", name.trim(), ext.trim());
                                println!("[SD]   {}", full);
                                mp3_files.push(full);
                            }
                        });
                        println!("[SD] {} files found", mp3_files.len());
                        let _ = volume_mgr.close_dir(mp3_dir);
                        sd_ok = true;
                    } else {
                        // Try lowercase
                        if let Ok(mp3_dir) = volume_mgr.open_dir(root_dir, "mp3") {
                            println!("[SD] Found /mp3/ folder");
                            let _ = volume_mgr.iterate_dir(mp3_dir, |entry| {
                                if !entry.attributes.is_directory() {
                                    let name = core::str::from_utf8(&entry.name.base_name()).unwrap_or("?");
                                    let ext = core::str::from_utf8(&entry.name.extension()).unwrap_or("");
                                    let full = alloc::format!("{}.{}", name.trim(), ext.trim());
                                    println!("[SD]   {}", full);
                                    mp3_files.push(full);
                                }
                            });
                            println!("[SD] {} files found", mp3_files.len());
                            let _ = volume_mgr.close_dir(mp3_dir);
                            sd_ok = true;
                        } else {
                            println!("[SD] No /mp3/ or /MP3/ folder");
                        }
                    }
                    let _ = volume_mgr.close_dir(root_dir);
                } else {
                    println!("[SD] Can't open root dir");
                }
            }
            Err(e) => {
                println!("[SD] Can't open volume: {:?}", e);
            }
            }
        }
        Err(_) => println!("[SD] No card"),
    }

    // === Audio (ES8311 codec + I2S) ===
    // CRITICAL ORDER:
    // 1. Keep PA amplifier DISABLED (GPIO46 LOW) - prevents white noise from floating I2S line
    // 2. Init codec (codec init leaves DAC powered but no input yet)
    // 3. Immediately mute the codec DAC + HP output
    // 4. Init I2S bus
    // Only when we actually beep: unmute codec -> raise PA_EN -> write DMA -> lower PA_EN -> mute
    println!("[AUDIO] Init codec...");
    let mut audio_codec = Es8311::new(RefCellDevice::new(&i2c_ref));
    let mut pa_en = Output::new(peripherals.GPIO46, Level::Low, OutputConfig::default());
    match audio_codec.init() {
        Ok(()) => println!("[AUDIO] Codec OK"),
        Err(_) => println!("[AUDIO] Codec FAILED"),
    }
    // Mute IMMEDIATELY after init, before any I2S traffic
    let _ = audio_codec.mute();

    // === I2S Audio Output (using public write_dma) ===
    println!("[AUDIO] Init I2S...");
    use esp_hal::i2s::master::{I2s, Config as I2sConfig, DataFormat};
    use esp_hal::dma::DmaDescriptor;
    let i2s_config = I2sConfig::default()
        .with_sample_rate(Rate::from_hz(16000))
        .with_data_format(DataFormat::Data16Channel16);
    let i2s_periph = I2s::new(peripherals.I2S0, peripherals.DMA_CH1, i2s_config)
        .expect("I2S failed")
        .with_mclk(peripherals.GPIO16);
    static mut I2S_TX_DESC: [DmaDescriptor; 8] = [DmaDescriptor::EMPTY; 8];
    let mut i2s_tx = i2s_periph.i2s_tx
        .with_bclk(peripherals.GPIO41)
        .with_ws(peripherals.GPIO45)
        .with_dout(peripherals.GPIO40)
        .build(unsafe { &mut I2S_TX_DESC });

    // Pre-generate beep sound (800Hz, 50ms, stereo 16-bit @ 16kHz = 3200 bytes)
    static mut BEEP_BUF: [u8; 4000] = [0u8; 4000];
    let beep_len = fill_beep_buffer(unsafe { &mut BEEP_BUF }, 800, 16000, 50);
    println!("[AUDIO] I2S OK ({} bytes beep)", beep_len);

    // === WiFi init (esp-radio) ===
    println!("[WIFI] Init radio...");
    static RADIO: StaticCell<esp_radio::Controller<'static>> = StaticCell::new();
    let radio_controller = RADIO.init(esp_radio::init().expect("esp-radio init failed"));
    let wifi_config = esp_radio::wifi::Config::default();
    let (mut wifi_controller, wifi_interfaces) = esp_radio::wifi::new(
        radio_controller,
        peripherals.WIFI,
        wifi_config,
    ).expect("WiFi init failed");

    // Configure STA mode — set your WiFi credentials here before building.
    use esp_radio::wifi::{ModeConfig, ClientConfig, AuthMethod};
    let client_config = ClientConfig::default()
        .with_ssid(alloc::string::String::from(env!("WIFI_SSID")))
        .with_password(alloc::string::String::from(env!("WIFI_PASS")))
        .with_auth_method(AuthMethod::WpaWpa2Personal);
    let mode_config = ModeConfig::Client(client_config);
    wifi_controller.set_config(&mode_config).expect("WiFi config failed");
    wifi_controller.start().expect("WiFi start failed");
    println!("[WIFI] Connecting...");

    // Connect async (non-blocking)
    match wifi_controller.connect_async().await {
        Ok(()) => println!("[WIFI] Connected!"),
        Err(e) => println!("[WIFI] Connect failed: {:?}", e),
    }

    // === Network Stack (DHCP) ===
    println!("[WIFI] Setting up network stack...");
    use embassy_net::{Config as NetConfig, StackResources, Runner};
    use static_cell::StaticCell;
use embassy_time::with_timeout;

    let net_config = NetConfig::dhcpv4(Default::default());
    static RESOURCES: StaticCell<StackResources<3>> = StaticCell::new();
    let resources = RESOURCES.init(StackResources::new());

    let (stack, runner) = embassy_net::new(wifi_interfaces.sta, net_config, resources, 12345u64);

    // Spawn network runner task
    _spawner.spawn(net_task(runner)).ok();

    // Wait for DHCP IP
    println!("[WIFI] Waiting for DHCP...");
    loop {
        if let Some(config) = stack.config_v4() {
            println!("[WIFI] IP: {}", config.address);
            break;
        }
        Timer::after(Duration::from_millis(100)).await;
    }

    // === NTP Time Sync ===
    // Simple NTP: connect to pool.ntp.org:123 and get time
    println!("[NTP] Syncing time...");
    match ntp_sync(stack, &mut rtc).await {
        Ok(()) => println!("[NTP] Time synced!"),
        Err(()) => println!("[NTP] Sync failed (using RTC time)"),
    }

    // BLE init removed: btdm_controller_init panics (-4) when coexisting with WiFi
    // without coex config. We don't use BLE features yet anyway.

    let mut boot_button = Input::new(peripherals.GPIO0, InputConfig::default().with_pull(Pull::Up));
    println!("=== All systems GO! (Embassy async) ===");

    // === State ===
    let mut watchface = WatchFace::new();
    watchface.wifi_connected = true; // WiFi already connected at this point
    let mut current_page = Page::Clock;
    let mut app_state = AppState::Watchface;
    let mut snake_game = SnakeGame::new();
    let mut game_2048 = Game2048::new();
    let mut tetris_game = TetrisGame::new();
    let mut flappy_game = FlappyGame::new();
    let mut maze_game = MazeGame::new();
    let mut launcher = Launcher::new();
    let mut settings_app = SettingsApp::new();
    let mut mp3_player = Mp3Player::new();
    let mut smarthome_app = SmartHomeApp::new();
    if !mp3_files.is_empty() {
        mp3_player.set_track_count(mp3_files.len());
        mp3_player.set_track_name(&mp3_files[0]);
    }
    let mut last_touch_y: u16 = 0;
    let mut last_touch_x: u16 = 0;
    let mut accel = (0.0f32, 0.0f32, 0.0f32);
    let mut gyro_data = (0i16, 0i16, 0i16);
    let mut imu_temp: i16 = 250;
    let mut batt_pct: u8 = 0;
    let mut batt_mv: u16 = 0;
    let mut charging = false;
    let mut page_dirty = true;
    let mut swiping = false;
    let mut last_interaction = Instant::now();
    // screen_state levels:
    //   3 = full bright (interactive)
    //   2 = dim (transition)
    //   1 = AOD (Always-On Display: super-dim, minimal HH:MM, 1 update / minute)
    //   0 = full off (DISPOFF + SLPIN)
    let mut screen_state: u8 = 3;
    // Tracks the last minute we rendered in AOD so we update the screen exactly
    // once per minute, not faster. Saves both DMA bandwidth and AMOLED current.
    let mut aod_last_minute: u8 = 99;
    let mut swipe_dir: i32 = 0;
    let mut swipe_start_x: i32 = 0;
    let pixel_count = board::LCD_WIDTH as usize * board::LCD_HEIGHT as usize;
    let mut snap_current: Vec<u16> = vec![0u16; pixel_count];
    let mut snap_target: Vec<u16> = vec![0u16; pixel_count];

    // Initial render
    if let Ok(pct) = power.get_battery_percent() {
        batt_pct = pct;
        batt_mv = power.get_battery_voltage().unwrap_or(0);
        charging = power.is_charging().unwrap_or(false);
        watchface.update_battery(batt_pct, batt_mv, charging);
    }
    if let Ok(dt) = rtc.get_time() {
        watchface.update_time(dt.hours, dt.minutes, dt.seconds);
        watchface.update_date(dt.day, dt.month, dt.year);
    }
    watchface.force_redraw();
    let _ = watchface.render(&mut fb);
    fb.flush(&mut display);

    // === Event-driven async main loop ===
    //
    // The loop sleeps the CPU between iterations using `select` over:
    //   * a periodic timer whose period depends on the current app/screen state
    //   * the touch interrupt line (GPIO38, async falling edge)
    //   * the boot button line (GPIO0, async falling edge)
    //
    // Tick budgets per state (only consumed when nothing else wakes us):
    //   * screen off                : 30 s   (deep idle, just to refresh battery%)
    //   * watchface clock, gyro off : 1 s    (only the seconds digit changes)
    //   * watchface clock, gyro on  : 33 ms  (smooth gyro ball)
    //   * sensors page              : 100 ms (10 Hz IMU)
    //   * launcher / settings / mp3 : 100 ms
    //   * Snake / 2048 / Tetris / Maze : 16 ms (~60 Hz)
    //   * Flappy                    : 8 ms   (high responsiveness for jumps)
    //
    // A finger held on the screen forces 16 ms tick regardless of state.
    //
    // When the user touches the screen or presses BOOT, the select returns immediately
    // and we process the input. With this design, the CPU is parked >99% of the time
    // while sitting on the watchface.
    use embassy_futures::select::select3;

    let mut next_rtc = Instant::now();
    let mut next_battery = Instant::now();
    let mut last_frame = Instant::now();
    let mut next_watchface_flush = Instant::now();
    let mut wifi_connected = true;
    let mut last_wifi_idle_check = Instant::now();
    // Power-down the IMU at boot — only enable when a consumer (gyro toggle, game, sensors page) needs it.
    let _ = imu.power_down();
    let mut imu_powered = false;
    // Tracks the previous-iteration state of the FT3168 INT line.
    // We need this to keep polling touch.poll() ONCE more after the finger lifts,
    // otherwise we miss the swipe-end event and pages stay stuck mid-drag.
    let mut was_touching = false;

    loop {
        // Pick a tick budget based on current state. This is the MAX time we'll
        // sleep without any external wake source.
        let touch_held = touch_int.is_low();
        let button_held = boot_button.is_low();

        let tick = if touch_held || button_held {
            // Something is currently being held: wake fast enough to track motion / detect
            // long-press, but no faster than necessary.
            Duration::from_millis(16) // ~60 Hz
        } else if screen_state == 0 {
            // Screen completely off: only wake every 30 s for housekeeping (battery refresh).
            // GPIO falling edges still wake us instantly.
            Duration::from_secs(30)
        } else if screen_state == 1 {
            // AOD mode: wake every 10 s to check if a new minute has started.
            // We don't need exactly 60 s precision because the user only sees minutes change.
            Duration::from_secs(10)
        } else {
            match app_state {
                AppState::Watchface => match current_page {
                    // Clock page: 1 Hz when gyro is off (only seconds change),
                    // 33 ms when gyro is on (smooth ball animation).
                    Page::Clock => if watchface.gyro_enabled {
                        Duration::from_millis(33)
                    } else {
                        Duration::from_secs(1)
                    },
                    Page::Sensors => Duration::from_millis(100), // 10 Hz IMU display
                    Page::System  => Duration::from_secs(2),     // basically static
                },
                AppState::Launcher | AppState::Settings | AppState::Mp3Player
                | AppState::SmartHome => Duration::from_millis(100),
                AppState::Flappy => Duration::from_millis(8),
                AppState::Snake | AppState::Game2048 | AppState::Tetris
                | AppState::Maze => Duration::from_millis(16),
            }
        };

        // Sleep until the tick budget elapses OR a falling edge arrives on touch / boot button.
        // Notes:
        //   * If a pin is already low, wait_for_falling_edge will not fire (no edge to wait for),
        //     but the tick above is short (16 ms) so we still wake reactively.
        //   * The futures from esp-hal install GPIO interrupts on creation and remove them on
        //     drop, so the executor parks the CPU between events: this is the main power win.
        let _ = select3(
            Timer::after(tick),
            touch_int.wait_for_falling_edge(),
            boot_button.wait_for_falling_edge(),
        ).await;

        let now = Instant::now();
        let dt_ms = (now - last_frame).as_millis() as u32;
        last_frame = now;

        // === Sensors (gated by need + screen state) ===
        // IMU only when an interactive consumer needs it (gyro enabled, IMU-driven game, sensors page).
        // When screen is off OR no consumer needs it, we power-down the IMU completely
        // (CTRL7 = 0). The QMI8658's gyro alone draws ~1.5 mA so this is a meaningful win.
        let need_imu = screen_state >= 2
            && (watchface.gyro_enabled
                || app_state == AppState::Maze
                || app_state == AppState::Tetris
                || app_state == AppState::Flappy
                || (app_state == AppState::Watchface && current_page == Page::Sensors));
        if need_imu && !imu_powered {
            let _ = imu.power_up();
            imu_powered = true;
        } else if !need_imu && imu_powered {
            let _ = imu.power_down();
            imu_powered = false;
        }
        if need_imu {
            if let Ok(a) = imu.read_accel() {
                accel = (a.x, a.y, a.z);
                watchface.update_accel(a.x, a.y, a.z);
            }
            if let Ok(g) = imu.read_gyro() {
                gyro_data = ((g.x * 10.0) as i16, (g.y * 10.0) as i16, (g.z * 10.0) as i16);
            }
            if let Ok(t) = imu.read_temperature() {
                imu_temp = (t * 10.0) as i16;
            }
        }

        // RTC: 1 Hz update is enough for a clock display. Skip when screen is off OR in AOD
        // (AOD updates the RTC manually once per minute).
        if screen_state >= 2 && now >= next_rtc {
            if let Ok(dt) = rtc.get_time() {
                watchface.update_time(dt.hours, dt.minutes, dt.seconds);
                watchface.update_date(dt.day, dt.month, dt.year);
            }
            next_rtc = now + Duration::from_secs(1);
        }

        // Battery: every 60 s normally, every 5 min when the screen is off
        // (we still check occasionally to track charge state and update on next wake).
        if now >= next_battery {
            if let Ok(pct) = power.get_battery_percent() {
                batt_pct = pct;
                batt_mv = power.get_battery_voltage().unwrap_or(0);
                charging = power.is_charging().unwrap_or(false);
                watchface.update_battery(batt_pct, batt_mv, charging);
            }
            next_battery = if screen_state == 0 {
                now + Duration::from_secs(300)
            } else {
                now + Duration::from_secs(60)
            };
        }

        // === Touch ===
        // Poll the I2C touch controller when:
        //   1. screen is on AND a finger is currently on the panel (INT low), OR
        //   2. screen is on AND the finger was on the panel last iteration (catches the
        //      lift/swipe-end event — without this, page swipes stay stuck mid-drag).
        // This keeps the bus quiet >99% of the time but never misses release events.
        let mut swipe_event = None;
        let mut tap_event = false;
        let int_low = touch_int.is_low();
        // Touch I2C is only polled in fully-interactive states (AOD has no touch handling).
        let touch_active = screen_state >= 2 && (int_low || was_touching);
        was_touching = int_low;
        if touch_active {
            if let Ok((point, event)) = touch.poll() {
            // Swipe handling for page navigation (only in Watchface mode)
            if app_state == AppState::Watchface {
                if let Some(tp) = point {
                    last_touch_x = tp.x;
                    last_touch_y = tp.y;
                    if !swiping {
                        if swipe_start_x == 0 { swipe_start_x = tp.x as i32; }
                        else {
                            let dx = tp.x as i32 - swipe_start_x;
                            if dx.unsigned_abs() > 30 {
                                swiping = true;
                                swipe_dir = if dx < 0 { -1 } else { 1 };
                                snap_current.copy_from_slice(fb.buffer());
                                let target = if swipe_dir < 0 { current_page.next() } else { current_page.prev() };
                                fb.clear_color(target.color());
                                match target {
                                    Page::Clock => {
                                        let mut wf2 = WatchFace::new();
                                        if let Ok(dt) = rtc.get_time() { wf2.update_time(dt.hours, dt.minutes, dt.seconds); }
                                        wf2.update_battery(batt_pct, batt_mv, charging);
                                        wf2.force_redraw();
                                        let _ = wf2.render(&mut fb);
                                    }
                                    Page::Sensors => { let _ = pages::draw_sensors_page(&mut fb, 0,0,0,0,0,0,0); }
                                    Page::System => { let _ = pages::draw_system_page(&mut fb, batt_mv, batt_pct, charging); }
                                }
                                snap_target.copy_from_slice(fb.buffer());
                            }
                        }
                    }
                    if swiping {
                        let delta = (tp.x as i32 - swipe_start_x).clamp(-(board::LCD_WIDTH as i32), board::LCD_WIDTH as i32);
                        let offset = ((delta * swipe_dir).clamp(0, board::LCD_WIDTH as i32) as usize) & !1;
                        let w = board::LCD_WIDTH as usize;
                        let h = board::LCD_HEIGHT as usize;
                        if offset > 0 && offset < w {
                            if swipe_dir < 0 {
                                display.set_addr_window(0, 0, (w-offset) as u16, h as u16);
                                display.bus_mut().begin_pixels();
                                for row in 0..h { display.bus_mut().stream_pixels(&snap_current[row*w+offset..row*w+w]); }
                                display.bus_mut().end_pixels();
                                display.set_addr_window((w-offset) as u16, 0, offset as u16, h as u16);
                                display.bus_mut().begin_pixels();
                                for row in 0..h { display.bus_mut().stream_pixels(&snap_target[row*w..row*w+offset]); }
                                display.bus_mut().end_pixels();
                            } else {
                                display.set_addr_window(0, 0, offset as u16, h as u16);
                                display.bus_mut().begin_pixels();
                                for row in 0..h { display.bus_mut().stream_pixels(&snap_target[row*w+w-offset..row*w+w]); }
                                display.bus_mut().end_pixels();
                                display.set_addr_window(offset as u16, 0, (w-offset) as u16, h as u16);
                                display.bus_mut().begin_pixels();
                                for row in 0..h { display.bus_mut().stream_pixels(&snap_current[row*w..row*w+w-offset]); }
                                display.bus_mut().end_pixels();
                            }
                        }
                    }
                }
                if let Some(swipe) = event {
                    swipe_start_x = 0;
                    if swiping {
                        swiping = false;
                        let ok = matches!(
                            (&swipe.direction, swipe_dir),
                            (SwipeDirection::Left, -1) | (SwipeDirection::Right, 1)
                        );
                        if ok {
                            if swipe_dir < 0 { current_page = current_page.next(); }
                            else { current_page = current_page.prev(); }
                            fb.buffer_mut().copy_from_slice(&snap_target);
                            page_dirty = true;
                        } else {
                            fb.buffer_mut().copy_from_slice(&snap_current);
                            fb.flush(&mut display);
                        }
                    } else {
                        swipe_event = Some(swipe.direction);
                        tap_event = swipe.direction == SwipeDirection::Tap;
                    }
                }
            } else {
                // In app mode: track position + forward events
                if let Some(tp) = point {
                    last_touch_x = tp.x;
                    last_touch_y = tp.y;
                }
                if let Some(swipe) = event {
                    swipe_event = Some(swipe.direction);
                    tap_event = swipe.direction == SwipeDirection::Tap;
                }
            }
            }
        }

        // === Screen sleep/wake state machine ===
        // Levels:
        //   3 = full bright + interactive (default)
        //   2 = dim brightness, still interactive (transition state at 20 s idle)
        //   1 = AOD: minimal HH:MM, super-dim, 1 update/min, no I/O
        //   0 = full off (DISPOFF + SLPIN), only GPIO interrupts can wake
        //
        // Transitions on idle: 3 → (20s) → 2 → (40s) → 1 (AOD) → (10min) → 0 (off)
        // Any touch/button bumps us straight back to 3.
        let any_touch = touch_int.is_low();
        if any_touch || swipe_event.is_some() || tap_event || boot_button.is_low() {
            last_interaction = now;
            if screen_state < 3 {
                // Wake up to full bright. If we were fully off (state 0), re-init the panel.
                if screen_state == 0 {
                    display.display_on();
                    Timer::after(Duration::from_millis(20)).await;
                }
                display.set_brightness(0xD0);
                screen_state = 3;
                next_watchface_flush = now;
                if app_state == AppState::Watchface {
                    watchface.force_redraw();
                    page_dirty = true;
                }
            }
        }
        let idle_secs = (now - last_interaction).as_secs();
        // 10 min in AOD → fully off
        if idle_secs >= 600 && screen_state > 0 {
            display.set_brightness(0x00);
            display.display_off();
            screen_state = 0;
        // 40 s idle → AOD (only when on the watchface — in apps we just dim/off)
        } else if idle_secs >= 40 && screen_state > 1 {
            if app_state == AppState::Watchface && current_page == Page::Clock {
                display.set_brightness(0x18); // very dim, ~10% of normal
                screen_state = 1;
                aod_last_minute = 99; // force first AOD frame
            } else {
                // Not on the clock face → no AOD, just go straight to off
                display.set_brightness(0x00);
                display.display_off();
                screen_state = 0;
            }
        // 20 s idle → dim transition
        } else if idle_secs >= 20 && screen_state > 2 {
            display.set_brightness(0x40);
            screen_state = 2;
        }

        // === WiFi auto-disconnect after long idle ===
        // The 2.4 GHz radio is the single biggest constant drain on the watch.
        // After 5 minutes of inactivity, drop the connection. The user can wake
        // the screen and we'll reconnect on next interaction (handled below).
        if wifi_connected && idle_secs >= 300
            && (now - last_wifi_idle_check).as_secs() >= 60 {
            last_wifi_idle_check = now;
            if wifi_controller.disconnect_async().await.is_ok() {
                println!("[WIFI] Disconnected (idle >5min, saving power)");
                wifi_connected = false;
                watchface.wifi_connected = false;
            }
        }
        // Reconnect when the user wakes the screen back up to full bright
        if !wifi_connected && screen_state == 3 && (now - last_wifi_idle_check).as_secs() >= 5 {
            last_wifi_idle_check = now;
            if wifi_controller.connect_async().await.is_ok() {
                println!("[WIFI] Reconnected on wake");
                wifi_connected = true;
                watchface.wifi_connected = true;
            }
        }

        // === AOD render path ===
        // In AOD we render *only* when the minute changes. Reads RTC, draws minimal
        // black-background HH:MM into the framebuffer, flushes once. Total work per minute:
        // ~1 RTC read + ~1 framebuffer fill + 1 DMA flush. The CPU sleeps the rest of the time.
        if screen_state == 1 {
            if let Ok(dt) = rtc.get_time() {
                if dt.minutes != aod_last_minute {
                    aod_last_minute = dt.minutes;
                    watchface.update_time(dt.hours, dt.minutes, dt.seconds);
                    if let Ok(pct) = power.get_battery_percent() {
                        watchface.update_battery(pct, batt_mv, charging);
                    }
                    let _ = watchface.render_aod(&mut fb);
                    fb.flush(&mut display);
                }
            }
            continue; // skip the normal app/render path
        }

        // When the screen is off, skip all rendering/flushing.
        // Keeps the QSPI bus idle so the CO5300 stays in a clean sleep state,
        // and the wake-up set_brightness/display_on commands always get through.
        if screen_state == 0 {
            continue;
        }

        // === App state machine ===
        match app_state {
            AppState::Watchface => {
                if !swiping {
                    let mut need_flush = false;
                    if page_dirty {
                        fb.clear_color(current_page.color());
                        match current_page {
                            Page::Clock => { watchface.force_redraw(); }
                            Page::System => { let _ = pages::draw_system_page(&mut fb, batt_mv, batt_pct, charging); }
                            _ => {}
                        }
                        page_dirty = false;
                        need_flush = true;
                    }
                    match current_page {
                        Page::Clock => {
                            // Only render if WatchFace says something is dirty.
                            if watchface.needs_render() {
                                let _ = watchface.render(&mut fb);
                                need_flush = true;
                            }
                        }
                        Page::Sensors => {
                            // Sensors page is repainted at the loop tick rate (10 Hz).
                            let ax = (accel.0 * 100.0) as i16;
                            let ay = (accel.1 * 100.0) as i16;
                            let az = (accel.2 * 100.0) as i16;
                            fb.clear_color(current_page.color());
                            let _ = pages::draw_sensors_page(&mut fb, ax, ay, az, gyro_data.0, gyro_data.1, gyro_data.2, imu_temp);
                            need_flush = true;
                        }
                        Page::System => {} // Static, already rendered
                    }
                    // Only flush if we actually drew something. The TE wait + 402 KB DMA
                    // is by far the heaviest periodic operation in the firmware, so we
                    // gate it strictly on dirtiness.
                    if need_flush {
                        fb.flush_vsync(&mut display, &te_pin);
                        next_watchface_flush = now;
                    }
                }

                // Tap on gyro zone → toggle gyro
                if tap_event && current_page == Page::Clock {
                    if WatchFace::is_gyro_zone(last_touch_y) {
                        let enabled = watchface.toggle_gyro();
                        println!("Gyro: {}", if enabled { "ON" } else { "OFF" });
                    }
                }

                // Swipe up on Clock → launcher, boot button too
                if let Some(SwipeDirection::Up) = swipe_event {
                    if current_page == Page::Clock {
                        app_state = AppState::Launcher;
                    }
                }
                if boot_button.is_low() {
                    app_state = AppState::Launcher;
                    Timer::after(Duration::from_millis(200)).await;
                }
            }

            AppState::Snake => {
                let prev_score = snake_game.score();
                let input = AppInput {
                    touch: None,
                    swipe: swipe_event,
                    tap: tap_event,
                    accel,
                    dt_ms: dt_ms.max(1),
                };
                match snake_game.update(&input) {
                    AppResult::Continue => {
                        if snake_game.stepped() {
                            snake_game.render(&mut fb);
                            fb.flush(&mut display);
                            // Beep when food eaten via I2S DMA
                            if snake_game.score() > prev_score {
                                // Unmute codec, then raise PA amplifier, then play
                                let _ = audio_codec.unmute();
                                delay.delay_millis(2); // let codec stabilize before enabling amp
                                pa_en.set_high();
                                if let Ok(transfer) = i2s_tx.write_dma(unsafe { &BEEP_BUF }) {
                                    let _ = transfer.wait();
                                }
                                // Lower amp FIRST, then mute codec to avoid pop
                                pa_en.set_low();
                                let _ = audio_codec.mute();
                            }
                        }
                    }
                    AppResult::Exit => {
                        app_state = AppState::Watchface;
                        watchface.force_redraw();
                        page_dirty = true;
                    }
                }

                if boot_button.is_low() {
                    app_state = AppState::Watchface;
                    watchface.force_redraw();
                    page_dirty = true;
                    Timer::after(Duration::from_millis(200)).await;
                }
            }

            AppState::Launcher => {
                // Track touch Y for tap detection
                if let Ok((point, _)) = touch.poll() {
                    if let Some(tp) = point { last_touch_y = tp.y; }
                }
                if let Some(new_state) = launcher.update(swipe_event, tap_event, last_touch_y) {
                    app_state = new_state;
                    match app_state {
                        AppState::Snake => snake_game.setup(),
                        AppState::Game2048 => { game_2048.setup(); game_2048.render(&mut fb); fb.flush(&mut display); }
                        AppState::Tetris => tetris_game.setup(),
                        AppState::Flappy => flappy_game.setup(),
                        AppState::Maze => maze_game.setup(),
                        AppState::Mp3Player => mp3_player.setup(),
                        AppState::SmartHome => smarthome_app.setup(),
                        AppState::Settings => {}
                        AppState::Watchface => { watchface.force_redraw(); page_dirty = true; }
                        _ => {}
                    }
                } else {
                    launcher.render(&mut fb);
                    fb.flush(&mut display);
                }
                if boot_button.is_low() {
                    app_state = AppState::Watchface;
                    watchface.force_redraw();
                    page_dirty = true;
                    Timer::after(Duration::from_millis(200)).await;
                }
            }

            AppState::Game2048 => {
                let input = AppInput { touch: None, swipe: swipe_event, tap: tap_event, accel, dt_ms: dt_ms.max(1) };
                game_2048.update(&input);
                // Only render on input (swipe moves tiles)
                if swipe_event.is_some() {
                    game_2048.render(&mut fb);
                    fb.flush_vsync(&mut display, &te_pin);
                }
                if boot_button.is_low() { app_state = AppState::Launcher; Timer::after(Duration::from_millis(200)).await; }
            }

            AppState::Tetris => {
                let input = AppInput { touch: None, swipe: swipe_event, tap: tap_event, accel, dt_ms: dt_ms.max(1) };
                tetris_game.update(&input);
                if tetris_game.stepped() || swipe_event.is_some() || tap_event {
                    tetris_game.render(&mut fb);
                    fb.flush_vsync(&mut display, &te_pin);
                }
                if boot_button.is_low() { app_state = AppState::Launcher; Timer::after(Duration::from_millis(200)).await; }
            }

            AppState::Flappy => {
                // Touch via GPIO38 (instant)
                let touch_down = touch_int.is_low();
                let fake_touch = if touch_down { Some(crate::peripherals::touch::TouchPoint { x: 200, y: 250, fingers: 1 }) } else { None };
                let input = AppInput { touch: fake_touch, swipe: swipe_event, tap: tap_event, accel, dt_ms: dt_ms.max(1) };
                flappy_game.update(&input);
                // Double-buffered render: draw to fb, swap+flush with VSync
                flappy_game.render(&mut fb);
                if now >= next_watchface_flush {
                    fb.swap_and_flush(&mut display, &te_pin);
                    next_watchface_flush = now + Duration::from_millis(33);
                }
                if boot_button.is_low() {
                    app_state = AppState::Launcher;
                    page_dirty = true;
                    Timer::after(Duration::from_millis(200)).await;
                }
            }

            AppState::Maze => {
                let input = AppInput { touch: None, swipe: swipe_event, tap: tap_event, accel, dt_ms: dt_ms.max(1) };
                maze_game.update(&input);
                // Maze renders at 30fps (IMU continuous)
                if now >= next_watchface_flush {
                    maze_game.render(&mut fb);
                    fb.flush_vsync(&mut display, &te_pin);
                    next_watchface_flush = now + Duration::from_millis(33);
                }
                if boot_button.is_low() { app_state = AppState::Launcher; Timer::after(Duration::from_millis(200)).await; }
            }

            AppState::SmartHome => {
                let input = AppInput { touch: None, swipe: swipe_event, tap: tap_event, accel, dt_ms: dt_ms.max(1) };
                smarthome_app.update(&input);
                // TODO: when get_pending_request() returns a URL, send HTTP request via embassy-net
                // For now just show the UI
                smarthome_app.render(&mut fb);
                if now >= next_watchface_flush {
                    fb.flush_vsync(&mut display, &te_pin);
                    next_watchface_flush = now + Duration::from_millis(100);
                }
                if boot_button.is_low() { app_state = AppState::Launcher; Timer::after(Duration::from_millis(200)).await; }
            }

            AppState::Mp3Player => {
                let input = AppInput { touch: None, swipe: swipe_event, tap: tap_event, accel, dt_ms: dt_ms.max(1) };
                mp3_player.update(&input);
                mp3_player.render(&mut fb);
                if now >= next_watchface_flush {
                    fb.flush_vsync(&mut display, &te_pin);
                    next_watchface_flush = now + Duration::from_millis(200);
                }
                if boot_button.is_low() { app_state = AppState::Launcher; Timer::after(Duration::from_millis(200)).await; }
            }

            AppState::Settings => {
                settings_app.update(dt_ms.max(1));
                // For T9: detect touch down via GPIO38 for rapid multi-tap
                if tap_event {
                    settings_app.handle_tap(last_touch_x, last_touch_y);
                }
                // Also read live touch position for keyboard area
                if let Ok((Some(tp), _)) = touch.poll() {
                    last_touch_x = tp.x;
                    last_touch_y = tp.y;
                }
                settings_app.render(&mut fb);
                if now >= next_watchface_flush {
                    fb.flush_vsync(&mut display, &te_pin);
                    next_watchface_flush = now + Duration::from_millis(50);
                }
                if boot_button.is_low() {
                    app_state = AppState::Launcher;
                    Timer::after(Duration::from_millis(200)).await;
                }
            }

            _ => {
                app_state = AppState::Watchface;
            }
        }
    }
}
