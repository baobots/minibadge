#![no_std]
#![no_main]

use core::f64;

use defmt::println;
use defmt::unwrap;
use embassy_executor::{InterruptExecutor, Spawner};
use embassy_rp::adc;
use embassy_rp::gpio::Input;
use embassy_rp::gpio::Pull;
use embassy_rp::interrupt;
use embassy_rp::interrupt::{InterruptExt, Priority};

use embassy_sync::pubsub::PubSubChannel;
use embassy_sync::pubsub::Publisher;
use log::{info, warn};

use embassy_rp::peripherals::PIO0;
use embassy_rp::pio::{InterruptHandler, Pio};
use embassy_rp::pwm;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;

use embassy_time::with_timeout;
use embassy_time::Instant;
use embassy_time::{Duration, Ticker, Timer};

use embassy_rp::bind_interrupts;
use heapless::Vec;
use infrared::{protocol::Nec, Receiver};
use panic_probe as _;

mod capnp;
mod rgbeffects;
mod scenes;
mod usb;
mod ws2812;

pub mod usb_messages_capnp {
    include!(concat!(env!("OUT_DIR"), "/usb_messages_capnp.rs"));
}

bind_interrupts!(struct Irqs {
    PIO0_IRQ_0 => InterruptHandler<PIO0>;
    ADC_IRQ_FIFO => adc::InterruptHandler;
});

use rand::rngs::SmallRng;
use rand::SeedableRng;
use rgbeffects::ColorPalette;
use rgbeffects::FragmentShader;
use rgbeffects::Pattern;
use rgbeffects::RenderCommand;
use rgbeffects::RenderManager;
use smart_leds::RGB8;
use ws2812::Ws2812;

const LED_MATRIX_WIDTH: usize = 3;
const LED_MATRIX_HEIGHT: usize = 3;
const LED_MATRIX_SIZE: usize = LED_MATRIX_WIDTH * LED_MATRIX_HEIGHT;

#[derive(Clone, Copy, Default, Debug)]
struct RawFramebuffer<T>
where
    T: Default + Copy,
{
    framebuffer: [T; LED_MATRIX_SIZE],
}

impl<T> RawFramebuffer<T>
where
    T: Default + Copy,
{
    fn new() -> Self {
        Self {
            framebuffer: [T::default(); LED_MATRIX_SIZE],
        }
    }

    fn set_pixel(&mut self, x: usize, y: usize, colour: T) {
        if x < LED_MATRIX_WIDTH && y < LED_MATRIX_HEIGHT {
            self.framebuffer[y * LED_MATRIX_WIDTH + x] = colour;
        }
    }

    fn get_pixel(&self, x: usize, y: usize) -> T {
        if x < LED_MATRIX_WIDTH && y < LED_MATRIX_HEIGHT {
            self.framebuffer[y * LED_MATRIX_WIDTH + x]
        } else {
            T::default()
        }
    }

    fn set_all(&mut self, rgb: T) {
        for i in 0..LED_MATRIX_SIZE {
            self.framebuffer[i] = rgb;
        }
    }

    fn get_raw(&self) -> &[T; LED_MATRIX_SIZE] {
        &self.framebuffer
    }
}

struct LedMatrix {
    raw_framebuffer: RawFramebuffer<RGB8>,
    gamma_corrected_framebuffer: RawFramebuffer<RGB8>,
    corrected_gain: f32,
    raw_gain: f32,
}

impl LedMatrix {
    fn new() -> Self {
        Self {
            raw_framebuffer: RawFramebuffer::new(),
            gamma_corrected_framebuffer: RawFramebuffer::new(),
            corrected_gain: 1.0,
            raw_gain: 1.0,
        }
    }

    fn set_gain(&mut self, gain: f32) {
        self.corrected_gain = gain;
    }

    fn set_raw_gain(&mut self, gain: f32) {
        self.raw_gain = gain;
    }

    fn get_pixel(&self, x: usize, y: usize) -> RGB8 {
        self.raw_framebuffer.get_pixel(x, y)
    }

    fn set_pixel(&mut self, x: usize, y: usize, colour: RGB8) {
        self.raw_framebuffer.set_pixel(x, y, colour);
    }

    fn update_gamma_correction_and_gain(&mut self) {
        static GAMMA_CORRECTION: [u8; 256] = [
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1,
            1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 2, 2, 2, 2, 2, 2, 2, 2, 3, 3, 3, 3, 3, 3, 3, 4, 4,
            4, 4, 4, 5, 5, 5, 5, 6, 6, 6, 6, 7, 7, 7, 7, 8, 8, 8, 9, 9, 9, 10, 10, 10, 11, 11, 11,
            12, 12, 13, 13, 13, 14, 14, 15, 15, 16, 16, 17, 17, 18, 18, 19, 19, 20, 20, 21, 21, 22,
            22, 23, 24, 24, 25, 25, 26, 27, 27, 28, 29, 29, 30, 31, 32, 32, 33, 34, 35, 35, 36, 37,
            38, 39, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 50, 51, 52, 54, 55, 56, 57, 58,
            59, 60, 61, 62, 63, 64, 66, 67, 68, 69, 70, 72, 73, 74, 75, 77, 78, 79, 81, 82, 83, 85,
            86, 87, 89, 90, 92, 93, 95, 96, 98, 99, 101, 102, 104, 105, 107, 109, 110, 112, 114,
            115, 117, 119, 120, 122, 124, 126, 127, 129, 131, 133, 135, 137, 138, 140, 142, 144,
            146, 148, 150, 152, 154, 156, 158, 160, 162, 164, 167, 169, 171, 173, 175, 177, 180,
            182, 184, 186, 189, 191, 193, 196, 198, 200, 203, 205, 208, 210, 213, 215, 218, 220,
            223, 225, 228, 231, 233, 236, 239, 241, 244, 247, 249, 252, 255,
        ];

        for i in 0..LED_MATRIX_SIZE {
            let colour = self.raw_framebuffer.framebuffer[i];

            let colour = RGB8 {
                r: (colour.r as f32 * self.corrected_gain) as u8,
                g: (colour.g as f32 * self.corrected_gain) as u8,
                b: (colour.b as f32 * self.corrected_gain) as u8,
            };

            let colour = RGB8 {
                r: GAMMA_CORRECTION[colour.r as usize],
                g: GAMMA_CORRECTION[colour.g as usize],
                b: GAMMA_CORRECTION[colour.b as usize],
            };

            let colour = RGB8 {
                r: (colour.r as f32 * self.raw_gain) as u8,
                g: (colour.g as f32 * self.raw_gain) as u8,
                b: (colour.b as f32 * self.raw_gain) as u8,
            };

            self.gamma_corrected_framebuffer.framebuffer[i] = colour;
        }
    }

    fn set_all(&mut self, rgb: RGB8) {
        self.raw_framebuffer.set_all(rgb);
    }

    fn get_gamma_corrected(&mut self) -> &[RGB8; LED_MATRIX_SIZE] {
        self.update_gamma_correction_and_gain();

        self.gamma_corrected_framebuffer.get_raw()
    }

    fn clear(&mut self) {
        self.set_all((0, 0, 0).into());
    }
}

#[derive(Clone, Debug)]
enum TaskCommand {
    ThermalThrottleMultiplier(f32), // 1.0 = no throttle, 0.0 = full throttle
    IrCommand(u8, u8, bool),        // add, cmd, repeat
    ShortButtonPress,
    LongButtonPress,
    MidiSetPixel(u8, u8, u8, u8), // x y channel (0=r 1=g 2=b) value
    SetWorkingMode(WorkingMode),
    SendIr(u8, u8, bool),
    IrTxDone,
    NextPattern,
    IncreaseBrightness,
    DecreaseBrightness,
    ResetTime,
    None,
}

static MEGA_CHANNEL: PubSubChannel<CriticalSectionRawMutex, TaskCommand, 8, 4, 8> =
    PubSubChannel::new();
type MegaPublisher = Publisher<'static, CriticalSectionRawMutex, TaskCommand, 8, 4, 8>;
type MegaSubscriber =
    embassy_sync::pubsub::Subscriber<'static, CriticalSectionRawMutex, TaskCommand, 8, 4, 8>;

// if we need to override the normal rendering with a special effect, we use this enum
#[derive(Clone, Debug)]
enum WorkingMode {
    Normal,                             // normal rendering, user selecting the patterns etc
    Special(RenderCommand), // override normal rendering until the user presses the button
    SpecialTimeout(RenderCommand, f64), // override normal rendering until the timeout
    RawFramebuffer(RawFramebuffer<RGB8>),
}
#[derive(Clone)]
enum OutputPower {
    High,
    Medium,
    Low,
    NighMode,
}

impl OutputPower {
    fn increase(&self) -> Self {
        match self {
            OutputPower::High => OutputPower::NighMode,
            OutputPower::Medium => OutputPower::High,
            OutputPower::Low => OutputPower::Medium,
            OutputPower::NighMode => OutputPower::Low,
        }
    }

    fn decrease(&self) -> Self {
        match self {
            OutputPower::High => OutputPower::Medium,
            OutputPower::Medium => OutputPower::Low,
            OutputPower::Low => OutputPower::NighMode,
            OutputPower::NighMode => OutputPower::High,
        }
    }
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    info!("Program start");
    println!("Program start");
    let p = embassy_rp::init(Default::default());

    let Pio {
        mut common, sm0, ..
    } = Pio::new(p.PIO0, Irqs);

    let adc = adc::Adc::new(p.ADC, Irqs, adc::Config::default());
    let ts = adc::Channel::new_temp_sensor(p.ADC_TEMP_SENSOR);
    unwrap!(spawner.spawn(temperature(adc, ts, MEGA_CHANNEL.publisher().unwrap())));

    let mut midi_framebuffer = RawFramebuffer::new();
    unwrap!(spawner.spawn(usb::usb_main(p.USB, MEGA_CHANNEL.publisher().unwrap())));

    let mut renderman = RenderManager {
        mtrx: LedMatrix::new(),
        rng: SmallRng::seed_from_u64(69420),
        persistent_data: Default::default(),
    };
    let mut ws2812 = Ws2812::new(&mut common, sm0, p.DMA_CH0, p.PIN_19);

    let patterns = scenes::PATTERNS.get();

    let boot_animation = RenderCommand {
        effect: Pattern::Animation(
            patterns.boot_animation,
            (patterns.boot_animation.len() as f32) * 2.0,
        ),
        color: ColorPalette::Rainbow(1.0),
        pattern_shaders: Vec::from_slice(&[FragmentShader::LowPassWithPeak(50.0)]).unwrap(),
        ..Default::default()
    };
    // override normal rendering with a special effect, if needed
    let mut working_mode = WorkingMode::SpecialTimeout(boot_animation.clone(), 0.5);

    let mut scene_id = 0;
    let mut out_power = OutputPower::High;

    let ir_sensor = Input::new(p.PIN_10, Pull::None);
    let mut user_button = Input::new(p.PIN_9, Pull::Up);

    // if we start with the button pressed, function as a torch light
    if user_button.is_low() {
        Timer::after_millis(100).await;
        out_power = OutputPower::High; // just to not forget to put this at the max value

        working_mode = WorkingMode::Special(RenderCommand {
            effect: Pattern::Simple(patterns.all_on),
            color: ColorPalette::Solid((255, 255, 255).into()),
            ..Default::default()
        });

        user_button.wait_for_high().await;
    }

    unwrap!(spawner.spawn(button_driver(
        user_button,
        MEGA_CHANNEL.publisher().unwrap()
    )));

    // infrared stuff

    interrupt::SWI_IRQ_1.set_priority(Priority::P3);
    let highpriority_spawner = EXECUTOR_HIGH.start(interrupt::SWI_IRQ_1);
    unwrap!(highpriority_spawner.spawn(ir_receiver(ir_sensor, MEGA_CHANNEL.publisher().unwrap())));

    let mut pwm_cfg: pwm::Config = Default::default();
    pwm_cfg.enable = false;
    let ir_blaster = pwm::Pwm::new_output_b(p.PWM_SLICE5, p.PIN_11, pwm_cfg);
    unwrap!(highpriority_spawner.spawn(ir_blaster_tsk(
        ir_blaster,
        MEGA_CHANNEL.subscriber().unwrap(),
        MEGA_CHANNEL.publisher().unwrap()
    )));

    let mut is_transmitting = false;

    let scenes = scenes::scenes();

    let mega_publisher = MEGA_CHANNEL.publisher().unwrap();
    let mut mega_subscriber = MEGA_CHANNEL.subscriber().unwrap();

    info!("Starting loop");
    mega_publisher
        .publish(TaskCommand::SendIr(0, 66, false))
        .await;

    let mut timer_offset = 0.0;
    loop {
        let t = Instant::now().as_micros() as f64 / 1_000_000.0 - timer_offset;

        match out_power {
            OutputPower::High => renderman.mtrx.set_gain(1.0),
            OutputPower::Medium => renderman.mtrx.set_gain(0.7),
            OutputPower::Low => renderman.mtrx.set_gain(0.5),
            OutputPower::NighMode => renderman.mtrx.set_gain(0.25),
        }

        if let Some(message) = mega_subscriber.try_next_message_pure() {
            info!("Handling message: {:?}", message);
            match message {
                TaskCommand::ThermalThrottleMultiplier(gain) => {
                    renderman.mtrx.set_raw_gain(gain);
                    if gain < 1.0 {
                        warn!("Thermal throttling! {}", gain);
                    }
                }
                TaskCommand::IrCommand(addr, cmd, repeat) => {
                    if is_transmitting {
                        warn!("Ignoring IR command, we are transmitting");
                        continue;
                    }

                    match (addr, cmd, repeat) {
                        // all those are commands of the chinese ir rgb remote
                        (0, 70, false) => {
                            mega_publisher
                                .publish(TaskCommand::DecreaseBrightness)
                                .await;
                        }
                        (0, 69, false) => {
                            mega_publisher
                                .publish(TaskCommand::IncreaseBrightness)
                                .await;
                        }

                        (0, 71, false) => { // off
                        }

                        (0, 67, false) => {
                            // on
                            // this is used to sync clocks between multiple devices
                            mega_publisher.publish(TaskCommand::ResetTime).await;
                        }

                        (0, 68, false) => {
                            // animations
                            mega_publisher.publish(TaskCommand::NextPattern).await;
                        }
                        // END of ir command from the chinese remote

                        // startup ir command sent by another badge
                        // say hi to the other badge
                        (0, 66, false) => {
                            // we do this so the animation starts in the correct time
                            mega_publisher.publish(TaskCommand::ResetTime).await;

                            mega_publisher
                                .publish(TaskCommand::SetWorkingMode(WorkingMode::SpecialTimeout(
                                    boot_animation.clone(),
                                    0.5,
                                )))
                                .await;
                        }

                        _ => {}
                    }
                }
                TaskCommand::ShortButtonPress => {
                    mega_publisher.publish(TaskCommand::NextPattern).await;
                }
                TaskCommand::LongButtonPress => {
                    mega_publisher
                        .publish(TaskCommand::DecreaseBrightness)
                        .await;
                }

                TaskCommand::MidiSetPixel(x, y, channel, value) => {
                    let px: RGB8 = midi_framebuffer.get_pixel(x as usize, y as usize);

                    let rgb = match channel {
                        0 => (value, px.g, px.b).into(),
                        1 => (px.r, value, px.b).into(),
                        2 => (px.r, px.g, value).into(),
                        _ => px,
                    };

                    midi_framebuffer.set_pixel(x as usize, y as usize, rgb);

                    working_mode = WorkingMode::RawFramebuffer(midi_framebuffer);
                }

                TaskCommand::SendIr(_, _, _) => {
                    is_transmitting = true;
                }

                TaskCommand::IrTxDone => {
                    is_transmitting = false;
                }

                TaskCommand::NextPattern => {
                    if let WorkingMode::Normal = working_mode {
                        scene_id = (scene_id + 1) % scenes.len();
                    } else {
                        working_mode = WorkingMode::Normal;
                    }
                }

                TaskCommand::IncreaseBrightness | TaskCommand::DecreaseBrightness => {
                    if let TaskCommand::DecreaseBrightness = message {
                        out_power = out_power.decrease();
                    } else {
                        out_power = out_power.increase();
                    }

                    let patt = match out_power {
                        OutputPower::High => patterns.power_100,
                        OutputPower::Medium => patterns.power_75,
                        OutputPower::Low => patterns.power_50,
                        OutputPower::NighMode => patterns.power_25,
                    };

                    // do not ruin the midi framebuffer
                    if !matches!(working_mode, WorkingMode::RawFramebuffer(_)) {
                        working_mode = WorkingMode::SpecialTimeout(
                            RenderCommand {
                                effect: Pattern::Simple(patt),
                                color: ColorPalette::Solid((255, 255, 255).into()),
                                ..Default::default()
                            },
                            t + 1.0,
                        );
                    }
                }

                TaskCommand::SetWorkingMode(wm) => {
                    working_mode = wm;
                }

                TaskCommand::ResetTime => {
                    timer_offset = Instant::now().as_micros() as f64 / 1_000_000.0;
                }

                TaskCommand::None => {}
            }
        }

        match &working_mode {
            WorkingMode::Normal => {
                renderman.render(&scenes[scene_id], t);
            }
            WorkingMode::SpecialTimeout(scene, timeout) => {
                renderman.render(&[scene.clone()], t);

                if t > *timeout {
                    working_mode = WorkingMode::Normal;
                }
            }
            WorkingMode::Special(scene) => {
                renderman.render(&[scene.clone()], t);
            }
            WorkingMode::RawFramebuffer(fb) => {
                renderman.mtrx.raw_framebuffer = *fb;
            }
        }

        ws2812.write(renderman.mtrx.get_gamma_corrected()).await;
        Timer::after_millis(1).await;
        renderman.mtrx.clear();
    }
}

static EXECUTOR_HIGH: InterruptExecutor = InterruptExecutor::new();

#[interrupt]
unsafe fn SWI_IRQ_1() {
    EXECUTOR_HIGH.on_interrupt()
}

#[embassy_executor::task]
async fn ir_receiver(ir_sensor: Input<'static>, publisher: MegaPublisher) {
    let mut int_receiver: Receiver<Nec, embassy_rp::gpio::Input> = Receiver::builder()
        .rc5()
        .frequency(1_000_000)
        .pin(ir_sensor)
        .protocol()
        .build();

    loop {
        int_receiver.pin_mut().wait_for_any_edge().await;

        if let Ok(Some(cmd)) = int_receiver.event_instant(Instant::now().as_ticks() as u32) {
            publisher
                .publish(TaskCommand::IrCommand(cmd.addr, cmd.cmd, cmd.repeat))
                .await;
        }
    }
}

#[embassy_executor::task]
async fn ir_blaster_tsk(
    mut ir_blaster: pwm::Pwm<'static>,
    mut subscriber: MegaSubscriber,
    publisher: MegaPublisher,
) {
    use infrared::sender::Status;

    fn enable_pwm(pwm: &mut pwm::Pwm, pwm_cfg: &mut pwm::Config, enable: bool) {
        pwm_cfg.enable = enable;
        pwm.set_config(pwm_cfg);

        // why the hell does the pwm pin stay high when we disable the pwm?
        unsafe {
            *((0x40014000 + 11 * 8 + 0x04) as *mut u32) = if enable { 4 } else { 0x1f };
        }
    }

    loop {
        if let TaskCommand::SendIr(addr, cmd, repeat) = subscriber.next_message_pure().await {
            const FREQUENCY: u32 = 20000;

            let mut buffer: infrared::sender::PulsedataSender<128> =
                infrared::sender::PulsedataSender::new();

            let cmd = infrared::protocol::nec::NecCommand { addr, cmd, repeat };
            buffer.load_command::<Nec, FREQUENCY>(&cmd);
            let mut counter = 0;

            let mut pwm_cfg: pwm::Config = Default::default();
            pwm_cfg.enable = false;
            // system clock is 125MHz
            // we need to do 38khz, so 125_000_000 / 38_000 = 3289
            pwm_cfg.top = (125_000_000 / 38_000) as u16;
            pwm_cfg.compare_b = pwm_cfg.top / 2;

            let mut ticker = Ticker::every(Duration::from_hz(FREQUENCY as u64));
            loop {
                let status: infrared::sender::Status = buffer.tick(counter);
                counter = counter.wrapping_add(1);

                match status {
                    Status::Transmit(v) => {
                        enable_pwm(&mut ir_blaster, &mut pwm_cfg, v);
                    }
                    Status::Idle => {
                        enable_pwm(&mut ir_blaster, &mut pwm_cfg, false);
                        break;
                    }
                    Status::Error => {
                        log::error!("Error in IR blaster");
                        enable_pwm(&mut ir_blaster, &mut pwm_cfg, false);
                        break;
                    }
                };

                ticker.next().await;
            }
            log::info!("tx done");
            enable_pwm(&mut ir_blaster, &mut pwm_cfg, false);
            publisher.publish(TaskCommand::IrTxDone).await;
        }
    }
}

#[embassy_executor::task]
async fn temperature(
    mut adc: adc::Adc<'static, adc::Async>,
    mut ts: adc::Channel<'static>,
    publisher: MegaPublisher,
) {
    let mut ticker = Ticker::every(Duration::from_secs(1));

    loop {
        let temp = match adc.read(&mut ts).await {
            Ok(v) => v,
            Err(e) => {
                log::error!("Error reading temperature: {:?}", e);
                continue;
            }
        };

        // TODO: yeah let's waste precious CPU cycles to calculate the temperature before checking if we need to throttle
        let adc_voltage = (3.3 / 4096.0) * temp as f64;
        let temp_degrees_c = 27.0 - (adc_voltage - 0.706) / 0.001721;

        if temp_degrees_c > 50.0 {
            // lerp from 55 to 65 degrees maps to gain from 1.0 to 0.1
            let gain: f64 = 1.0 - (temp_degrees_c - 55.0) / 10.0;
            let gain = gain.clamp(0.0, 1.0);
            publisher
                .publish(TaskCommand::ThermalThrottleMultiplier(gain as f32))
                .await;
        }

        ticker.next().await;
    }
}

#[embassy_executor::task]
async fn button_driver(mut button: Input<'static>, publisher: MegaPublisher) {
    let mut press_start;

    loop {
        button.wait_for_low().await;
        press_start = Instant::now();

        match with_timeout(Duration::from_millis(1000), button.wait_for_high()).await {
            // no timeout
            Ok(_) => {}
            // timeout
            Err(_) => {
                publisher.publish(TaskCommand::LongButtonPress).await;
                button.wait_for_high().await;
            }
        }

        let press_duration = Instant::now() - press_start;

        if press_duration >= Duration::from_millis(50)
            && press_duration < Duration::from_millis(1000)
        {
            publisher.publish(TaskCommand::ShortButtonPress).await;
        }
    }
}
