#![no_std]
#![no_main]

use embassy_executor::Spawner;
use embassy_futures::join::join;
use embassy_futures::select::select;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::Timer;
use esp_backtrace as _;
use esp_hal::clock::CpuClock;
use esp_hal::gpio::{Level, Output, OutputConfig};
use esp_hal::timer::timg::TimerGroup;
use esp_radio::ble::controller::BleConnector;
use log::{info, warn};
use trouble_host::prelude::*;

esp_bootloader_esp_idf::esp_app_desc!();

const DEVICE_NAME: &str = "Beeper";
const CONNECTIONS_MAX: usize = 1;
const L2CAP_CHANNELS_MAX: usize = 2;

static BLINK_SIGNAL: Signal<CriticalSectionRawMutex, ()> = Signal::new();

// Custom BLE service for the Beeper device.
// Service UUID 0xBEEF, characteristic UUID 0xBEE0 (write to trigger LED blink).
#[gatt_server]
struct BeeperServer {
    beeper_service: BeeperService,
}

#[gatt_service(uuid = "0000BEEF-0000-1000-8000-00805F9B34FB")]
struct BeeperService {
    #[characteristic(uuid = "0000BEE0-0000-1000-8000-00805F9B34FB", write, write_without_response)]
    trigger: u8,
}

#[esp_rtos::main]
async fn main(_s: Spawner) {
    esp_println::logger::init_logger_from_env();

    let peripherals = esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::max()));
    esp_alloc::heap_allocator!(size: 72 * 1024);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let software_interrupt =
        esp_hal::interrupt::software::SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, software_interrupt.software_interrupt0);

    let led = Output::new(peripherals.GPIO7, Level::Low, OutputConfig::default());

    let connector = BleConnector::new(peripherals.BT, Default::default()).unwrap();
    let controller: ExternalController<_, 20> = ExternalController::new(connector);

    join(run_ble(controller), led_task(led)).await;
}

async fn run_ble<C: Controller>(controller: C) {
    let address = Address::random([0xBE, 0xEF, 0xC0, 0xFF, 0xEE, 0x01]);
    info!("BLE address: {:?}", address);

    let mut resources: HostResources<DefaultPacketPool, CONNECTIONS_MAX, L2CAP_CHANNELS_MAX> =
        HostResources::new();
    let stack = trouble_host::new(controller, &mut resources).set_random_address(address);
    let host = stack.build();
    let runner = host.runner;
    let mut peripheral = host.peripheral;

    let server = BeeperServer::new_with_config(GapConfig::Peripheral(PeripheralConfig {
        name: DEVICE_NAME,
        appearance: &appearance::UNKNOWN,
    }))
    .unwrap();

    join(ble_runner_task(runner), async {
        loop {
            info!("Advertising as '{}'", DEVICE_NAME);
            match advertise(&mut peripheral, &server).await {
                Ok(conn) => {
                    info!("Central connected");
                    select(gatt_events_task(&server, &conn), core::future::pending::<()>()).await;
                    info!("Central disconnected");
                }
                Err(_) => {}
            }
        }
    })
    .await;
}

async fn ble_runner_task<C: Controller, P: PacketPool>(mut runner: Runner<'_, C, P>) {
    loop {
        if runner.run().await.is_err() {
            break;
        }
    }
}

async fn advertise<'v, 's, C: Controller>(
    peripheral: &mut Peripheral<'v, C, DefaultPacketPool>,
    server: &'s BeeperServer<'v>,
) -> Result<GattConnection<'v, 's, DefaultPacketPool>, BleHostError<C::Error>> {
    let mut adv_data = [0u8; 31];
    // 16-bit service UUID 0xBEEF in little-endian so the app can filter by service UUID.
    let len = AdStructure::encode_slice(
        &[
            AdStructure::Flags(LE_GENERAL_DISCOVERABLE | BR_EDR_NOT_SUPPORTED),
            AdStructure::ServiceUuids16(&[[0xEF, 0xBE]]),
            AdStructure::CompleteLocalName(DEVICE_NAME.as_bytes()),
        ],
        &mut adv_data[..],
    )?;
    let advertiser = peripheral
        .advertise(
            &Default::default(),
            Advertisement::ConnectableScannableUndirected {
                adv_data: &adv_data[..len],
                scan_data: &[],
            },
        )
        .await?;
    let conn = advertiser.accept().await?.with_attribute_server(server)?;
    Ok(conn)
}

async fn gatt_events_task<P: PacketPool>(
    server: &BeeperServer<'_>,
    conn: &GattConnection<'_, '_, P>,
) -> Result<(), Error> {
    let trigger = server.beeper_service.trigger;
    loop {
        match conn.next().await {
            GattConnectionEvent::Disconnected { reason } => {
                info!("[gatt] disconnected: {:?}", reason);
                break;
            }
            GattConnectionEvent::Gatt { event } => {
                if let GattEvent::Write(ref ev) = event {
                    if ev.handle() == trigger.handle {
                        info!("[gatt] trigger write — signalling LED blink");
                        BLINK_SIGNAL.signal(());
                    }
                }
                match event.accept() {
                    Ok(reply) => reply.send().await,
                    Err(e) => warn!("[gatt] reply error: {:?}", e),
                }
            }
            _ => {}
        }
    }
    Ok(())
}

// Waits for BLINK_SIGNAL then toggles the LED every 250 ms for 3 seconds.
async fn led_task(mut led: Output<'_>) -> ! {
    loop {
        BLINK_SIGNAL.wait().await;
        info!("LED blink start");
        for _ in 0..12 {
            led.toggle();
            Timer::after_millis(250).await;
        }
        led.set_low();
        info!("LED blink done");
    }
}
