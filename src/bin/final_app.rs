// 开发辅助说明：AI 补全请用中文回答。
#![no_std]
#![no_main]

extern crate alloc; // 开启动态内存支持，用于格式化字符串

use alloc::format; // 引入 format! 宏
use core::net::Ipv4Addr;
use embassy_executor::Spawner;
use embassy_net::{tcp::TcpSocket, Runner, StackResources};
use embassy_time::{Duration, Timer};
use esp_alloc as _;
use esp_backtrace as _;
#[cfg(target_arch = "riscv32")]
use esp_hal::interrupt::software::SoftwareInterruptControl;
// 引入 GPIO 和 Delay 相关的库
use esp_hal::{
    clock::CpuClock,
    delay::Delay,
    gpio::{Flex, InputConfig, OutputConfig, Pull},
    ram,
    rng::Rng,
    timer::timg::TimerGroup,
};
use esp_println::println;
use esp_radio::{
    wifi::{
        ClientConfig, ModeConfig, ScanConfig, WifiController, WifiDevice, WifiEvent, WifiStaState,
    },
    Controller,
};

esp_bootloader_esp_idf::esp_app_desc!();

// 内存静态化宏
macro_rules! mk_static {
    ($t:ty,$val:expr) => {{
        static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        #[deny(unused_attributes)]
        let x = STATIC_CELL.uninit().write(($val));
        x
    }};
}

const SSID: &str = "XiongLab_p2_2.4G";
const PASSWORD: &str = "Xiong123";

// ==========================================
//  移植过来的 1-Wire 驱动 (不用改动)
// ==========================================
struct OneWire<'d> {
    pin: Flex<'d>,
    delay: Delay,
}

impl<'d> OneWire<'d> {
    fn new(mut pin: Flex<'d>, delay: Delay) -> Self {
        // 预设输出低，开启输入，初始状态关闭输出(释放/High)
        pin.set_low();
        pin.set_input_enable(true);
        pin.set_output_enable(false);
        // 如果有外部上拉电阻，这行其实可以去掉；没有则保留
        // pin.set_pull(Pull::Up);
        Self { pin, delay }
    }

    #[inline(always)]
    fn drive_low(&mut self) {
        self.pin.set_output_enable(true);
    }

    #[inline(always)]
    fn release_high(&mut self) {
        self.pin.set_output_enable(false);
    }

    // 复位脉冲
    fn reset(&mut self) -> bool {
        critical_section::with(|_| {
            self.drive_low();
            self.delay.delay_micros(480);
            self.release_high();
            self.delay.delay_micros(60); // 稍微调小了一点等待时间，适配 C6
            let presence = self.pin.is_low();
            self.delay.delay_micros(420);
            presence
        })
    }

    // 写 Bit
    fn write_bit(&mut self, bit: bool) {
        critical_section::with(|_| {
            self.drive_low();
            if bit {
                self.delay.delay_micros(6);
                self.release_high();
                self.delay.delay_micros(64);
            } else {
                self.delay.delay_micros(60);
                self.release_high();
                self.delay.delay_micros(10);
            }
        });
    }

    // 读 Bit
    fn read_bit(&mut self) -> bool {
        critical_section::with(|_| {
            self.drive_low();
            self.delay.delay_micros(6);
            self.release_high();
            self.delay.delay_micros(9);
            let bit = self.pin.is_high();
            self.delay.delay_micros(55);
            bit
        })
    }

    fn write_byte(&mut self, byte: u8) {
        for i in 0..8 {
            self.write_bit((byte >> i) & 0x01 != 0);
        }
    }

    fn read_byte(&mut self) -> u8 {
        let mut byte = 0;
        for i in 0..8 {
            if self.read_bit() {
                byte |= 1 << i;
            }
        }
        byte
    }
}

// ==========================================
//  主程序 (Main)
// ==========================================

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    esp_println::logger::init_logger_from_env();
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    // 1. 初始化内存
    esp_alloc::heap_allocator!(#[ram(reclaimed)] size: 64 * 1024);
    esp_alloc::heap_allocator!(size: 36 * 1024);

    // 2. 初始化 RTOS 和定时器
    let timg0 = TimerGroup::new(peripherals.TIMG0);
    #[cfg(target_arch = "riscv32")]
    let sw_int = SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    esp_rtos::start(
        timg0.timer0,
        #[cfg(target_arch = "riscv32")]
        sw_int.software_interrupt0,
    );

    // 3. 初始化 1-Wire 传感器 (GPIO 10)
    // 注意：Delay 用于微秒级操作，不会阻塞 Wi-Fi 任务
    let delay_driver = Delay::new();
    let one_wire_pin = Flex::new(peripherals.GPIO10);
    let mut sensor = OneWire::new(one_wire_pin, delay_driver);

    // 4. 初始化 Wi-Fi
    let esp_radio_ctrl = &*mk_static!(Controller<'static>, esp_radio::init().unwrap());
    let (controller, interfaces) =
        esp_radio::wifi::new(&esp_radio_ctrl, peripherals.WIFI, Default::default()).unwrap();
    let wifi_interface = interfaces.sta;
    let config = embassy_net::Config::dhcpv4(Default::default());
    let rng = Rng::new();
    let seed = (rng.random() as u64) << 32 | rng.random() as u64;

    let (stack, runner) = embassy_net::new(
        wifi_interface,
        config,
        mk_static!(StackResources<3>, StackResources::<3>::new()),
        seed,
    );

    // 启动后台任务
    spawner.spawn(connection(controller)).ok();
    spawner.spawn(net_task(runner)).ok();

    let mut rx_buffer = [0; 4096];
    let mut tx_buffer = [0; 4096];

    // 等待连接
    println!("Waiting for WiFi link up...");
    loop {
        if stack.is_link_up() {
            break;
        }
        Timer::after(Duration::from_millis(500)).await;
    }

    println!("Waiting to get IP address...");
    loop {
        if let Some(config) = stack.config_v4() {
            println!("Got IP: {}", config.address);
            break;
        }
        Timer::after(Duration::from_millis(500)).await;
    }

    // ==========================================
    //  主循环：读温度 -> 发请求
    // ==========================================
    loop {
        println!("--- Starting new measurement loop ---");

        // 额外自检：确认 Wi-Fi 已连接且拿到 IP，避免在断线时忙尝试
        if !stack.is_link_up() {
            println!("[WARN] Wi-Fi link down，暂不发送。");
            Timer::after(Duration::from_secs(5)).await;
            continue;
        }
        if let Some(cfg) = stack.config_v4() {
            println!("[INFO] 当前 IP: {}", cfg.address);
        } else {
            println!("[WARN] 尚未获得 DHCP 地址，稍后重试。");
            Timer::after(Duration::from_secs(5)).await;
            continue;
        }

        // --- 步骤 A: 读取温度 ---
        let mut temperature = 0.0;
        let mut success = false;

        // 1. 复位 & 发起转换
        if sensor.reset() {
            sensor.write_byte(0xCC); // Skip ROM
            sensor.write_byte(0x44); // Convert T

            // 2. [关键] 异步等待转换完成
            // 这里我们不使用 sensor.delay (它是死等)，而是用 Timer::after (异步等待)
            // 这样在等待的 800ms 里，Wi-Fi 还能处理后台数据
            Timer::after(Duration::from_millis(800)).await;

            // 3. 读取数据
            sensor.reset();
            sensor.write_byte(0xCC);
            sensor.write_byte(0xBE);

            let lsb = sensor.read_byte();
            let msb = sensor.read_byte();
            let raw_temp = ((msb as u16) << 8) | (lsb as u16);
            temperature = raw_temp as f32 / 16.0;
            success = true;
            println!("Read Temp: {:.2} C", temperature);
        } else {
            println!("Sensor not found!");
        }

        // --- 步骤 B: 发送 HTTP 请求 ---
        if success {
            let mut socket = TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer);
            socket.set_timeout(Some(embassy_time::Duration::from_secs(10)));

            // 你的服务器地址 (请确认 IP 和端口是否正确)
            let remote_endpoint = (Ipv4Addr::new(159, 75, 201, 91), 5005);

            println!("Connecting to server...");
            match socket.connect(remote_endpoint).await {
                Ok(_) => {
                    println!("Connected!");

                    // 1. 动态构建 JSON 内容
                    let json_body = format!(
                        "{{\"temp\":{:.2}, \"co2\":null, \"time\":null}}",
                        temperature
                    );

                    // 2. 动态构建 HTTP 请求头
                    // 注意：必须计算正确的 Content-Length，否则服务器可能不认
                    let request = format!(
                        "POST /upload HTTP/1.1\r\n\
                        Host: 159.75.201.91\r\n\
                        Content-Type: application/json\r\n\
                        Content-Length: {}\r\n\
                        \r\n\
                        {}",
                        json_body.len(),
                        json_body
                    );

                    // 3. 发送
                    if let Err(e) = socket.write(request.as_bytes()).await {
                        println!("Write error: {:?}", e);
                    } else {
                        println!("Data sent: {}", json_body);
                    }

                    // 4. 读取响应 (可选，读一下确认服务器收到了)
                    let mut buf = [0; 1024];
                    match socket.read(&mut buf).await {
                        Ok(n) if n > 0 => {
                            if let Ok(resp) = core::str::from_utf8(&buf[..n]) {
                                println!("Server response: {}", resp);
                            }
                        }
                        _ => println!("No response or read error"),
                    }
                }
                Err(e) => println!("Connect error: {:?}", e),
            }
        }

        // 每 300 秒 (5分钟) 采集发送一次
        Timer::after(Duration::from_secs(300)).await;
        // // 每 10 秒 采集发送一次
        // Timer::after(Duration::from_secs(10)).await;
    }
}

// ----------------------------------------------------------------
//  以下是 Embassy 的后台任务 (不用改)
// ----------------------------------------------------------------

#[embassy_executor::task]
async fn connection(mut controller: WifiController<'static>) {
    loop {
        match esp_radio::wifi::sta_state() {
            WifiStaState::Connected => {
                println!("[WiFi] 已连接，等待断开事件以便重连监控。");
                controller.wait_for_event(WifiEvent::StaDisconnected).await;
                println!("[WiFi] 检测到断开，5 秒后重试连接。");
                Timer::after(Duration::from_millis(5000)).await
            }
            _ => {}
        }
        if !matches!(controller.is_started(), Ok(true)) {
            let client_config = ModeConfig::Client(
                ClientConfig::default()
                    .with_ssid(SSID.into())
                    .with_password(PASSWORD.into()),
            );
            controller.set_config(&client_config).unwrap();
            controller.start_async().await.unwrap();
        }
        match controller.connect_async().await {
            Ok(_) => println!("Wifi connected!"),
            Err(e) => {
                println!("[WiFi] 连接失败：{:?}，5 秒后重试。", e);
                Timer::after(Duration::from_millis(5000)).await
            }
        }
    }
}

#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, WifiDevice<'static>>) {
    runner.run().await
}
