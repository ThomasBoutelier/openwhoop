#[macro_use]
extern crate log;

use std::time::Duration;

use anyhow::anyhow;
use btleplug::{
    api::{BDAddr, Central, Manager as _, Peripheral as _, ScanFilter},
    platform::{Adapter, Manager, Peripheral},
};
use clap::{Parser, Subcommand};
use dotenv::dotenv;
use openwhoop::{
    algo::{ExerciseMetrics, SleepConsistencyAnalyzer},
    types::activities::{ActivityType, SearchActivityPeriods},
    DatabaseHandler, OpenWhoop, WhoopDevice,
};
use tokio::time::sleep;
use whoop::{constants::WHOOP_SERVICE, WhoopPacket};

#[derive(Parser)]
pub struct OpenWhoopCli {
    #[arg(env, long)]
    pub database_url: String,
    #[arg(env, long)]
    pub ble_interface: Option<String>,
    #[clap(subcommand)]
    pub subcommand: OpenWhoopCommand,
}

#[derive(Subcommand)]
pub enum OpenWhoopCommand {
    ///
    /// Scan for Whoop devices
    ///
    Scan,
    ///
    /// Download history data from whoop devices
    ///
    DownloadHistory {
        #[arg(long, env)]
        whoop_addr: BDAddr,
    },
    ///
    /// Reruns the packet processing on stored packets
    /// This is used after new more of packets get handled
    ///
    ReRun,
    ///
    /// Detects sleeps and exercises
    ///
    DetectEvents,
    ///
    /// Print sleep statistics for all time and last week
    ///
    SleepStats,
    ///
    /// Print activity statistics for all time and last week
    ///
    ExerciseStats,
    ///
    /// Calculate stress for historical data
    ///
    CalculateStress,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if let Err(error) = dotenv() {
        println!("{}", error);
    }

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .filter_module("sqlx::query", log::LevelFilter::Off)
        .filter_module("sea_orm_migration::migrator", log::LevelFilter::Off)
        .init();

    let cli = OpenWhoopCli::parse();
    let db_handler = DatabaseHandler::new(cli.database_url).await;

    let manager = Manager::new().await?;
    let adapter = match cli.ble_interface {
        Some(interface) => {
            let adapters = manager.adapters().await?;
            let mut c_adapter = Err(anyhow!("Adapter: `{}` not found", interface));
            for adapter in adapters {
                let name = adapter.adapter_info().await?;
                if name.starts_with(&interface) {
                    c_adapter = Ok(adapter);
                    break;
                }
            }

            c_adapter?
        }
        None => {
            let adapters = manager.adapters().await?;
            adapters
                .into_iter()
                .next()
                .ok_or(anyhow!("No BLE adapters found"))?
        }
    };

    match cli.subcommand {
        OpenWhoopCommand::Scan => {
            scan_command(adapter, None).await?;
            Ok(())
        }
        OpenWhoopCommand::DownloadHistory { whoop_addr } => {
            let peripheral = scan_command(adapter, Some(whoop_addr)).await?;
            let mut whoop = WhoopDevice::new(peripheral, db_handler);

            whoop.connect().await?;
            whoop.initialize().await?;

            let result = whoop.sync_history().await;
            if let Err(e) = result {
                error!("{}", e);
            }

            loop {
                if let Ok(true) = whoop.is_connected().await {
                    whoop
                        .send_command(WhoopPacket::exit_high_freq_sync())
                        .await?;
                    break;
                } else {
                    whoop.connect().await?;
                    sleep(Duration::from_secs(1)).await;
                }
            }

            Ok(())
        }
        OpenWhoopCommand::ReRun => {
            let whoop = OpenWhoop::new(db_handler.clone());
            let mut id = 0;
            loop {
                let packets = db_handler.get_packets(id).await?;
                if packets.is_empty() {
                    break;
                }

                for packet in packets {
                    id = packet.id;
                    whoop.handle_packet(packet).await?;
                }

                println!("{}", id);
            }

            Ok(())
        }
        OpenWhoopCommand::DetectEvents => {
            let whoop = OpenWhoop::new(db_handler);
            whoop.detect_sleeps().await?;
            whoop.detect_events().await?;
            Ok(())
        }
        OpenWhoopCommand::SleepStats => {
            let whoop = OpenWhoop::new(db_handler);
            let sleep_records = whoop.database.get_sleep_cycles().await?;
            let mut last_week = sleep_records
                .iter()
                .rev()
                .take(7)
                .copied()
                .collect::<Vec<_>>();

            last_week.reverse();
            let analyzer = SleepConsistencyAnalyzer::new(sleep_records);
            let metrics = analyzer.calculate_consistency_metrics();
            println!("All time: \n{}", metrics);
            let analyzer = SleepConsistencyAnalyzer::new(last_week);
            let metrics = analyzer.calculate_consistency_metrics();
            println!("Week: \n{}", metrics);

            Ok(())
        }
        OpenWhoopCommand::ExerciseStats => {
            let whoop = OpenWhoop::new(db_handler);
            let exercises = whoop
                .database
                .search_activities(
                    SearchActivityPeriods::default().with_activity(ActivityType::Activity),
                )
                .await?;

            let last_week = exercises
                .iter()
                .rev()
                .take(7)
                .copied()
                .rev()
                .collect::<Vec<_>>();

            let metrics = ExerciseMetrics::new(exercises);
            let last_week = ExerciseMetrics::new(last_week);

            println!("All time: \n{}", metrics);
            println!("Last week: \n{}", last_week);
            Ok(())
        }
        OpenWhoopCommand::CalculateStress => {
            let whoop = OpenWhoop::new(db_handler);
            whoop.calculate_stress().await?;
            Ok(())
        }
    }
}

async fn scan_command(
    adapter: Adapter,
    peripheral_addr: Option<BDAddr>,
) -> anyhow::Result<Peripheral> {
    adapter
        .start_scan(ScanFilter {
            services: vec![WHOOP_SERVICE],
        })
        .await?;

    loop {
        let peripherals = adapter.peripherals().await?;

        for peripheral in peripherals {
            let Some(properties) = peripheral.properties().await? else {
                continue;
            };

            if !properties.services.contains(&WHOOP_SERVICE) {
                continue;
            }

            let Some(peripheral_addr) = peripheral_addr else {
                println!("Address: {}", properties.address);
                println!("Name: {:?}", properties.local_name);
                println!("RSSI: {:?}", properties.rssi);
                println!();
                continue;
            };

            if properties.address == peripheral_addr {
                return Ok(peripheral);
            }
        }

        sleep(Duration::from_secs(1)).await;
    }
}
