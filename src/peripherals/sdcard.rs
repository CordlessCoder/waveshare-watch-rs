// SD Card driver via SPI (SPI3)
// Pins: MOSI=GPIO1(CMD), SCK=GPIO2(CLK), MISO=GPIO3(DATA), CS=GPIO17(SDCS)
// Uses embedded-sdmmc for FAT filesystem

/// SD card state
pub enum SdState {
    NotInserted,
    Mounted,
    Error,
}

// The SD card will be initialized in main.rs using:
// - esp_hal::spi::master::Spi on SPI3
// - embedded_sdmmc::SdCard wrapper
// - embedded_sdmmc::VolumeManager for FAT access
//
// Usage:
// let sd_spi = Spi::new(peripherals.SPI3, spi_config)
//     .with_sck(peripherals.GPIO2)
//     .with_mosi(peripherals.GPIO1)
//     .with_miso(peripherals.GPIO3);
// let sd_cs = Output::new(peripherals.GPIO17, Level::High, OutputConfig::default());
// let sd = embedded_sdmmc::SdCard::new(ExclusiveDevice::new(sd_spi, sd_cs), delay);
// let mut volume_mgr = embedded_sdmmc::VolumeManager::new(sd, DummyTimesource);
