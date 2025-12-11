#![no_std]
#![no_main]

use esp_backtrace as _;
use esp_hal::{
    clock::CpuClock,
    main,
    uart::{Config as UartConfig, DataBits, Parity, StopBits, Uart},
};
use esp_println::println;

esp_bootloader_esp_idf::esp_app_desc!();

#[main]
fn main() -> ! {
    // 跟你之前 temp_sensor.rs 一样的风格：用 esp_hal::Config 初始化
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    // 配置 UART：9600, 8N1，对应 CO2 模块协议
    let uart_cfg = UartConfig::default()
        .with_baudrate(9_600) // 9600 bps
        .with_data_bits(DataBits::_8)
        .with_parity(Parity::None)
        .with_stop_bits(StopBits::_1);

    // 用 UART0 + GPIO4 做 RX(模块 B 脚接这里，注意中间要分压)
    //
    // 注意：
    // - 这个 API 是 esp-hal 1.0 系的，如果编译器说签名不对，
    //   可以对照你当前版本的 Uart 示例，把 Config / with_rx 的写法调一下。
    let mut uart = Uart::new(peripherals.UART0, uart_cfg)
        .unwrap()
        .with_rx(peripherals.GPIO4);

    println!("CO2 传感器读取程序启动，正在等待数据流...");

    let mut frame = [0u8; 6];
    let mut buf1 = [0u8; 1];

    loop {
        // 1)先从串口里“捞”到一个 0x2C，当成帧头
        loop {
            if let Ok(n) = uart.read(&mut buf1) {
                if n == 0 {
                    // 当前没有数据，继续转一圈
                    continue;
                }

                if buf1[0] == 0x2C {
                    frame[0] = 0x2C;
                    break;
                }
                // 否则丢掉，继续找下一个字节
            }
        }

        // 2)已经拿到了 B1 = 0x2C，再读后面 5 个字节
        for i in 1..6 {
            loop {
                if let Ok(n) = uart.read(&mut buf1) {
                    if n == 0 {
                        continue;
                    } else {
                        frame[i] = buf1[0];
                        break;
                    }
                }
            }
        }

        let b1 = frame[0];
        let b2 = frame[1];
        let b3 = frame[2];
        let b4 = frame[3];
        let b5 = frame[4];
        let b6 = frame[5];

        // 3)检查满量程字段是否是固定值 0x03, 0xFF
        if b4 != 0x03 || b5 != 0xFF {
            println!(
                "CO2 帧满量程字段异常: b4=0x{:02X}, b5=0x{:02X}, frame={:02X?}",
                b4, b5, frame
            );
            continue;
        }

        // 4)Checksum 校验
        let sum = b1
            .wrapping_add(b2)
            .wrapping_add(b3)
            .wrapping_add(b4)
            .wrapping_add(b5);

        if sum != b6 {
            println!(
                "CO2 校验失败: 期望=0x{:02X}, 实际=0x{:02X}, frame={:02X?}",
                sum, b6, frame
            );
            continue;
        }

        // 5)转换为 ppm
        let co2_ppm: u16 = ((b2 as u16) << 8) | (b3 as u16);

        println!("CO2 = {} ppm (帧: {:02X?})", co2_ppm, frame);

        // 模块本身一般 1~2 秒发一帧，这里就不另外 delay 了，
        // 想 10 秒打印一次的话，可以自己加一个计数器或者 Delay。
    }
}
