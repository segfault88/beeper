#![no_std]
#![no_main]

use embassy_futures::join::join;
use embassy_futures::select::{select, Either};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::Timer;
use esp_backtrace as _;
use esp_hal::clock::CpuClock;
use esp_hal::gpio::{Level, Output, OutputConfig};
use esp_hal::rmt::{Channel, PulseCode, Rmt, Tx, TxChannelConfig, TxChannelCreator};
use esp_hal::time::Rate;
use esp_hal::timer::timg::TimerGroup;
use esp_hal::Blocking;
use esp_radio::ble::controller::BleConnector;
use log::{info, warn};
use static_cell::StaticCell;
use trouble_host::prelude::*;

esp_bootloader_esp_idf::esp_app_desc!();

const DEVICE_NAME: &str = "Beeper";
const CONNECTIONS_MAX: usize = 1;
const L2CAP_CHANNELS_MAX: usize = 2;

static BLINK_SIGNAL: Signal<CriticalSectionRawMutex, ()> = Signal::new();
static TOGGLE_SIGNAL: Signal<CriticalSectionRawMutex, bool> = Signal::new();

// Custom BLE service for the Beeper device.
// Service UUID 0xBEEF, characteristic UUID 0xBEE0 (write to trigger LED blink).
#[gatt_server]
struct BeeperServer {
    beeper_service: BeeperService,
}

#[gatt_service(uuid = "0000BEEF-0000-1000-8000-00805F9B34FB")]
struct BeeperService {
    #[characteristic(
        uuid = "0000BEE0-0000-1000-8000-00805F9B34FB",
        write,
        write_without_response
    )]
    trigger: u8,
    #[characteristic(
        uuid = "0000BEE1-0000-1000-8000-00805F9B34FB",
        write,
        write_without_response
    )]
    toggle: u8,
}

// Sync entry point — heap must be initialized before the Embassy executor starts,
// because esp_rtos::embassy::Executor::new() allocates BLE OS task stacks via
// InternalMemory. Using #[esp_rtos::main] hides the executor construction inside
// the macro and runs it before the async body, so the heap would be uninitialized.
#[esp_hal::main]
fn main() -> ! {
    esp_alloc::heap_allocator!(size: 128 * 1024);
    esp_println::logger::init_logger_from_env();

    let peripherals = esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::max()));

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let software_interrupt =
        esp_hal::interrupt::software::SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, software_interrupt.software_interrupt0);

    let led = Output::new(peripherals.GPIO7, Level::Low, OutputConfig::default());
    let connector = BleConnector::new(peripherals.BT, Default::default()).unwrap();
    let controller: ExternalController<_, 20> = ExternalController::new(connector);

    // WS2812B on GPIO8 (onboard RGB LED on ESP32-C6 DevKitC).
    // 80 MHz clock / divider 1 = 12.5 ns per tick.
    let rmt = Rmt::new(peripherals.RMT, Rate::from_mhz(80)).unwrap();
    let rgb = rmt
        .channel0
        .configure_tx(&TxChannelConfig::default().with_clk_divider(1))
        .unwrap()
        .with_pin(peripherals.GPIO8);

    static EXECUTOR: StaticCell<esp_rtos::embassy::Executor> = StaticCell::new();
    let executor = EXECUTOR.init(esp_rtos::embassy::Executor::new());
    executor.run(|spawner| {
        spawner.spawn(run(led, rgb, controller).unwrap());
    });
}

#[embassy_executor::task]
async fn run(
    led: Output<'static>,
    rgb: Channel<'static, Blocking, Tx>,
    controller: ExternalController<BleConnector<'static>, 20>,
) {
    join(run_ble(controller), led_task(led, rgb)).await;
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
                    select(
                        gatt_events_task(&server, &conn),
                        core::future::pending::<()>(),
                    )
                    .await;
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
    let toggle = server.beeper_service.toggle;
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
                    } else if ev.handle() == toggle.handle {
                        let on = ev.data().first().copied().unwrap_or(0) != 0;
                        info!("[gatt] toggle write — LED {}", if on { "on" } else { "off" });
                        TOGGLE_SIGNAL.signal(on);
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

// Write a single WS2812B pixel. WS2812B uses GRB order.
// Clock: 80 MHz / divider 1 = 12.5 ns/tick.
// T0: high 32 ticks (400 ns), low 68 ticks (850 ns).
// T1: high 64 ticks (800 ns), low 36 ticks (450 ns).
fn ws2812_write(
    channel: Channel<'static, Blocking, Tx>,
    r: u8,
    g: u8,
    b: u8,
) -> Channel<'static, Blocking, Tx> {
    // GRB wire order, MSB first
    let bits: u32 = ((g as u32) << 16) | ((r as u32) << 8) | (b as u32);

    let mut pulses = [PulseCode::end_marker(); 25]; // 24 data bits + terminator
    for i in 0..24 {
        pulses[i] = if (bits >> (23 - i)) & 1 != 0 {
            PulseCode::new(Level::High, 64, Level::Low, 36) // T1
        } else {
            PulseCode::new(Level::High, 32, Level::Low, 68) // T0
        };
    }

    match channel.transmit(&pulses) {
        Ok(tx) => match tx.wait() {
            Ok(ch) | Err((_, ch)) => ch,
        },
        Err((_, ch)) => ch,
    }
}

// Dim green when idle; bright green when toggled on; alternates yellow/purple during blink.
// Blink always returns to whatever the current toggle state is.
async fn led_task(mut led: Output<'_>, mut rgb: Channel<'static, Blocking, Tx>) -> ! {
    let mut led_on = false;
    rgb = ws2812_write(rgb, 0, 8, 0); // dim green — ready

    let set_idle = |ch: Channel<'static, Blocking, Tx>, on: bool| {
        if on {
            ws2812_write(ch, 0, 60, 0) // bright green — toggled on
        } else {
            ws2812_write(ch, 0, 8, 0) // dim green — idle
        }
    };

    loop {
        match select(BLINK_SIGNAL.wait(), TOGGLE_SIGNAL.wait()).await {
            Either::First(()) => {
                info!("LED blink start");
                // 40 × 125 ms = 5 s, doubled flash rate vs original 12 × 250 ms
                for i in 0..40 {
                    led.toggle();
                    rgb = if i % 2 == 0 {
                        ws2812_write(rgb, 60, 50, 0) // yellow
                    } else {
                        ws2812_write(rgb, 30, 0, 60) // purple
                    };
                    Timer::after_millis(125).await;
                }
                if led_on { led.set_high() } else { led.set_low() }
                rgb = set_idle(rgb, led_on);
                info!("LED blink done");
            }
            Either::Second(on) => {
                led_on = on;
                if on { led.set_high() } else { led.set_low() }
                rgb = set_idle(rgb, led_on);
            }
        }
    }
}
