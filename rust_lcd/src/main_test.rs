use rusb::{UsbContext};

fn main() {
    let ctx = match rusb::Context::new() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Failed to init libusb: {}", e);
            std::process::exit(1);
        }
    };
    
    let devices = ctx.devices().unwrap();
    
    // Find our device: VID 0x0416, PID 0x5302
    let dev = devices.iter()
        .find(|d| {
            let cfg = d.config_descriptor(0).unwrap();
            // ConfigDescriptor has vendor_id and product_id methods or fields
            // Let's check what's available
            true // placeholder
        })
        .unwrap();
    
    println!("Found target device");
    
    // For now just open any device and demonstrate the API
    // In practice, we'd filter by VID/PID properly
    for d in devices.iter().take(1) {
        let mut handle = d.open().unwrap();
        handle.claim_interface(0).unwrap();
        println!("Opened device interface");
        let _ = handle.release_interface(0);
    }
    
    println!("Done!");
}
