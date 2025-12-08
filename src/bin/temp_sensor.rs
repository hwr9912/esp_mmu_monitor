#![no_std]
#![no_main]

use esp_backtrace as _;
use esp_hal::{
    clock::CpuClock,
    delay::Delay,
    gpio::{Flex, Pull}, // 只需要 Flex 和 Pull
    main,
};
use esp_println::println;

esp_bootloader_esp_idf::esp_app_desc!();

// --- 手写 1-Wire 驱动 (基于使能位切换) ---

struct OneWire<'d> {
    pin: Flex<'d>,
    delay: Delay,
}

impl<'d> OneWire<'d> {
    fn new(mut pin: Flex<'d>, delay: Delay) -> Self {
        // 1. 预设输出电平为低。
        // 以后我们只控制“输出开关”，不控制“输出电平”。
        // 开关一开就是低，开关一关就是高阻（被电阻拉高）。
        pin.set_low();

        // 2. 开启输入功能 (永远开启，这样我们随时能读取线上的状态)
        pin.set_input_enable(true);

        // 3. 初始状态：关闭输出 (相当于释放总线，High)
        pin.set_output_enable(false);

        // 4. 尝试开启内部上拉 (可选)
        // 既然你有板载电阻，如果这行报错，可以直接删掉。
        // 在新版 HAL 中，通常是 set_pull
        // pin.set_pull(Pull::Up);

        Self { pin, delay }
    }

    // 动作：拉低总线
    #[inline(always)]
    fn drive_low(&mut self) {
        // 打开输出使能 -> 因为 set_low() 了，所以引脚变低
        self.pin.set_output_enable(true);
    }

    // 动作：释放总线
    #[inline(always)]
    fn release_high(&mut self) {
        // 关闭输出使能 -> 引脚浮空 -> 被电阻拉高
        self.pin.set_output_enable(false);
    }

    // 复位脉冲
    fn reset(&mut self) -> bool {
        critical_section::with(|_| {
            self.drive_low();
            self.delay.delay_micros(480); // 拉低 480us

            self.release_high();
            self.delay.delay_micros(70); // 释放后等待 70us 采样

            // 因为 set_input_enable(true) 常开，直接读就行
            let presence = self.pin.is_low();

            self.delay.delay_micros(410); // 等待时隙结束
            presence
        })
    }

    // 写一个 Bit
    fn write_bit(&mut self, bit: bool) {
        critical_section::with(|_| {
            self.drive_low(); // 开始：先拉低

            if bit {
                // 写 1: 拉低很短时间 (6us)，然后释放
                self.delay.delay_micros(6);
                self.release_high();
                self.delay.delay_micros(64);
            } else {
                // 写 0: 拉低很长时间 (60us)
                self.delay.delay_micros(60);
                self.release_high();
                self.delay.delay_micros(10);
            }
        });
    }

    // 读一个 Bit
    fn read_bit(&mut self) -> bool {
        critical_section::with(|_| {
            self.drive_low(); // 开始：拉低
            self.delay.delay_micros(6); // 保持 >1us

            self.release_high(); // 释放
            self.delay.delay_micros(9); // 等待数据稳定

            let bit = self.pin.is_high(); // 采样

            self.delay.delay_micros(55); // 等待时隙结束
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

#[main]
fn main() -> ! {
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    let delay = Delay::new();
    let pin = Flex::new(peripherals.GPIO10); // 你的数据引脚

    let mut ow = OneWire::new(pin, delay);

    println!("DS18B20 Raw Enable-Bit Demo...");

    loop {
        // 1. 复位
        if !ow.reset() {
            println!("Sensor not found!");
            ow.delay.delay_millis(1000);
            continue;
        }

        // 2. 发送指令
        ow.write_byte(0xCC); // Skip ROM
        ow.write_byte(0x44); // Convert T

        // 3. 等待转换
        // DS18B20 转换时如果不接强上拉，拉低总线可能导致转换失败
        // 所以我们这里只是死等，保持总线释放状态(High)
        ow.delay.delay_millis(800);

        // 4. 读取数据
        ow.reset();
        ow.write_byte(0xCC); // Skip ROM
        ow.write_byte(0xBE); // Read Scratchpad

        let lsb = ow.read_byte();
        let msb = ow.read_byte();

        let raw_temp = ((msb as u16) << 8) | (lsb as u16);
        let temperature = raw_temp as f32 / 16.0;

        println!("Temp: {:.2} C (Raw: {:04x})", temperature, raw_temp);
    }
}
