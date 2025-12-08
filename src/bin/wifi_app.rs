//! Embassy DHCP Example
//!
//!
//! Set SSID and PASSWORD env variable before running this example.
//!
//! This gets an ip address via DHCP then performs an HTTP get request to some
//! "random" server

#![no_std]
#![no_main]

use core::net::Ipv4Addr;
use embassy_executor::Spawner;
use embassy_net::{tcp::TcpSocket, Runner, StackResources};
use embassy_time::{Duration, Timer};
use esp_alloc as _;
use esp_backtrace as _;
#[cfg(target_arch = "riscv32")]
use esp_hal::interrupt::software::SoftwareInterruptControl;
use esp_hal::{clock::CpuClock, ram, rng::Rng, timer::timg::TimerGroup};
use esp_println::println;
use esp_radio::{
    wifi::{
        ClientConfig, ModeConfig, ScanConfig, WifiController, WifiDevice, WifiEvent, WifiStaState,
    },
    Controller,
};

esp_bootloader_esp_idf::esp_app_desc!();

// When you are okay with using a nightly compiler it's better to use https://docs.rs/static_cell/2.1.0/static_cell/macro.make_static.html
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

#[esp_rtos::main] // 指定这是基于 RTOS (FreeRTOS) 的入口点
async fn main(spawner: Spawner) -> ! {
    esp_println::logger::init_logger_from_env(); // 初始化日志系统
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config); // 初始化芯片外设，设置 CPU 为最高频率

    // 初始化堆内存 (Heap)。WiFi 驱动非常消耗内存，必须分配足够的堆空间
    esp_alloc::heap_allocator!(#[ram(reclaimed)] size: 64 * 1024);
    esp_alloc::heap_allocator!(size: 36 * 1024);

    // 初始化定时器 (Timer) 和软件中断，这是 Embassy 运行异步任务所必须的
    let timg0 = TimerGroup::new(peripherals.TIMG0);
    #[cfg(target_arch = "riscv32")]
    // RISC-V 架构的中断处理
    let sw_int = SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    // 启动底层的 RTOS 调度器
    esp_rtos::start(
        timg0.timer0,
        #[cfg(target_arch = "riscv32")]
        sw_int.software_interrupt0,
    );

    let esp_radio_ctrl = &*mk_static!(Controller<'static>, esp_radio::init().unwrap());

    // 初始化 WiFi 控制器
    let (controller, interfaces) =
        esp_radio::wifi::new(&esp_radio_ctrl, peripherals.WIFI, Default::default()).unwrap();
    // 获取 Station 接口 (作为客户端连接路由器的接口)
    let wifi_interface = interfaces.sta;
    // 配置网络栈使用 DHCP (自动获取 IP)
    let config = embassy_net::Config::dhcpv4(Default::default());
    // 生成随机种子 (用于 TCP 序列号等)
    let rng = Rng::new();
    let seed = (rng.random() as u64) << 32 | rng.random() as u64;

    // 初始化 Embassy 网络栈
    let (stack, runner) = embassy_net::new(
        wifi_interface,
        config,
        mk_static!(StackResources<3>, StackResources::<3>::new()),
        seed,
    );

    // 启动 WiFi 连接管理任务 (负责扫描和连接 WiFi)
    spawner.spawn(connection(controller)).ok();
    // 启动网络协议栈后台任务 (负责处理 TCP/IP 数据包)
    spawner.spawn(net_task(runner)).ok();

    let mut rx_buffer = [0; 4096];
    let mut tx_buffer = [0; 4096];

    // 等待物理/链路连接 (Link Up)
    println!("Waiting for WiFi link up...");
    loop {
        if stack.is_link_up() {
            break;
        }
        Timer::after(Duration::from_millis(500)).await;
    }

    // 等待路由器通过 DHCP 协议 给 ESP32 分配一个有效的 IP 地址
    println!("Waiting to get IP address...");
    loop {
        if let Some(config) = stack.config_v4() {
            println!("Got IP: {}", config.address);
            break;
        }
        Timer::after(Duration::from_millis(500)).await;
    }

    loop {
        // Embassy 的异步计时器, 停30秒
        Timer::after(Duration::from_secs(30)).await;
        // 创建 TCP socket
        let mut socket = TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer);
        // 设置 socket 超时
        socket.set_timeout(Some(embassy_time::Duration::from_secs(10)));
        // 设置远端服务器地址和端口号
        // let remote_endpoint = (Ipv4Addr::new(142, 250, 185, 115), 80);
        let remote_endpoint = (Ipv4Addr::new(159, 75, 201, 91), 5005);
        // 尝试连接 TCP
        println!("connecting...");
        let r = socket.connect(remote_endpoint).await;
        if let Err(e) = r {
            println!("connect error: {:?}", e);
            continue;
        }
        println!("connected!");
        // 创建接收缓冲区
        let mut buf = [0; 1024];
        // 一个小循环：发 HTTP → 再读回数据
        loop {
            // use embedded_io_async::Write;
            let r = socket
                .write(
                    b"POST /upload HTTP/1.1
                Host: x.x.x.x
                Content-Type: application/json
                Content-Length: 35

                {\"temp\":23.2,\"co2\":780,\"time\":1}",
                )
                .await;
            // 读写失败就 break 出去
            if let Err(e) = r {
                println!("write error: {:?}", e);
                break;
            }
            let n = match socket.read(&mut buf).await {
                Ok(0) => {
                    println!("read EOF");
                    break;
                }
                Ok(n) => n,
                Err(e) => {
                    println!("read error: {:?}", e);
                    break;
                }
            };
            println!("{}", core::str::from_utf8(&buf[..n]).unwrap());
        }
        // 每轮结束前再睡 3 秒
        Timer::after(Duration::from_millis(3000)).await;
    }
}

#[embassy_executor::task]
async fn connection(mut controller: WifiController<'static>) {
    println!("start connection task");
    println!("Device capabilities: {:?}", controller.capabilities());
    loop {
        match esp_radio::wifi::sta_state() {
            WifiStaState::Connected => {
                // wait until we're no longer connected
                controller.wait_for_event(WifiEvent::StaDisconnected).await;
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
            println!("Starting wifi");
            controller.start_async().await.unwrap();
            println!("Wifi started!");

            println!("Scan");
            let scan_config = ScanConfig::default().with_max(10);
            let result = controller
                .scan_with_config_async(scan_config)
                .await
                .unwrap();
            for ap in result {
                println!("{:?}", ap);
            }
        }
        println!("About to connect...");

        match controller.connect_async().await {
            Ok(_) => println!("Wifi connected!"),
            Err(e) => {
                println!("Failed to connect to wifi: {e:?}");
                Timer::after(Duration::from_millis(5000)).await
            }
        }
    }
}

#[embassy_executor::task]
async fn net_task(mut runner: Runner<'static, WifiDevice<'static>>) {
    runner.run().await
}
