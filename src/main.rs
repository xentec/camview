use std::{
	sync::Once,
	thread, time
};
use sixtyfps::{self, ComponentHandle, Weak};

use clap::Parser;
use serde::{Deserialize};
use chrono::{self, Datelike, Timelike};
use image;

use tokio;
use reqwest as http;
use futures_util::TryFutureExt;


use tracing as log;
use tracing_subscriber;

use anyhow::{Result, Context};
use sys_locale;

mod ui {
	sixtyfps::include_modules!();
}

#[derive(Parser, Debug)]
#[clap(about, version)]
struct Opt
{
	/// URL of the webcam
	#[clap(default_value = "http://othcam.oth-regensburg.de/webcam/Regensburg")]
	url: reqwest::Url,
}


fn main() -> Result<(), Box<dyn std::error::Error>>
{
	let opt = Opt::parse();

	tracing_subscriber::fmt()
		.with_max_level(log::Level::DEBUG)
		.compact()
		.init();

	let ui = ui::App::new();
	let io_task_handle = thread::Builder::new()
		.name("io-runtime".into())
		.spawn({
			let ui = ui.as_weak();
			move ||
			if let Err(err) = io_runtime_run(ui.clone(), opt).context("fatal error")
			{
				log::error!("{:#}", err);
				ui.upgrade_in_event_loop(move |ui| {
					ui.set_status(error_showable(err));
				});
			}
		})?;

	let _cleanup_task_handle = thread::Builder::new()
		.name("cleanup".into())
		.spawn({
			let ui = ui.as_weak();
			move ||
				if let Err(err) = io_task_handle.join()
					.map_err(|_err| anyhow::anyhow!("PANIC!"))
				{
					log::error!("{:#}", err);
					thread::sleep(time::Duration::from_secs(1));
					ui.upgrade_in_event_loop(move |ui| {
						let err_str = error_showable(err);
						ui.set_status(err_str);
					});
				}
		});

	ui.run();
	Ok(())
}

fn io_runtime_run(ui: Weak<ui::App>, opt: Opt) -> Result<()>
{
	use tokio::*;

	let rt = runtime::Builder::new_current_thread()
		.enable_all()
		.build()?;

	rt.block_on(io_run(ui, opt))?;
	rt.shutdown_timeout(time::Duration::from_secs(10));

	Ok(())
}

async fn io_run(ui: Weak<ui::App>, opt: Opt) -> Result<()>
{
	use tokio::*;

	let cam_name = opt.url.path_segments()
		.and_then(|mut segs| segs.next_back().or(segs.next_back()))
		.to_owned()
		.context("invalid URL path")?;

	let mut url_base = opt.url.clone();
	url_base.path_segments_mut().unwrap()
		.pop_if_empty();

	let url_list = {
		let mut url = url_base.join("include/list.php").context("failed to create URL")?;
		url.query_pairs_mut().append_pair("wc", &cam_name);
		url
	};

	let client = http::Client::new();
	let _updater: task::JoinHandle<Result<()>> = spawn({
		let ui = ui.clone();
		let client = client.clone();
		async move {
			let mut url_img_prev = String::new();		
			loop {
				log::debug!("Loading image metadata from {} ...", &url_list);
				
				#[derive(Deserialize)]
				struct List {
					hugeimg: String,
				}
				let res: Result<List> = client.get(url_list.clone())
					.send()
					.and_then(http::Response::json)
					.await.context("failed to fetch json, reloading in 30s");

				let url_img = match res {
					Ok(v) => format!("{}/{}", url_base, v.hugeimg),
					Err(err) => {
						log::error!("{:#}", err);
						ui.clone().upgrade_in_event_loop(move |ui| {
							ui.set_status(error_showable(err));
						});
						time::sleep(time::Duration::from_secs(30)).await;
						continue;
					},
				};

				if url_img_prev == url_img {
					time::sleep(time::Duration::from_secs(120)).await;
					continue;
				}

				url_img_prev = url_img.clone();
				let res = client.get(url_img)
					.send()
					.and_then(|resp| async { resp.error_for_status() })
					.and_then(|resp| resp.bytes())
					.await.context("failed to fetch image")
					.and_then(|bytes| image::load_from_memory_with_format(&bytes, image::ImageFormat::Jpeg).context("failed to parse JPEG image"))
					.context("failed to load image");

				let image = match res {
					Ok(resp) => resp.into_rgb8(),
					Err(err) => {
						log::error!("{:#}", err);
						ui.clone().upgrade_in_event_loop(move |ui| {
							ui.set_status(error_showable(err));
						});
						time::sleep(time::Duration::from_secs(30)).await;
						continue;
					},					
				};

				ui.clone().upgrade_in_event_loop(move |ui| {
					let pixels = sixtyfps::SharedPixelBuffer::clone_from_slice(image.as_raw(), image.width() as _, image.height() as _);
					ui.set_status(Default::default());
					ui.set_bg_image(sixtyfps::Image::from_rgb8(pixels));
					
				});
				time::sleep(time::Duration::from_secs(120)).await;
			}
		}
	});

	let time_task = spawn({
		let ui = ui.clone();
		async move {
			loop {
				let now = chrono::Local::now();
				let new_time = now.time().format("%H:%M").to_string();
				let new_date = {
					let date = now.date();
					if date.month() == 1 {
						date.format_localized("%A, %e. %B %Y", locale())
					} else {
						date.format_localized("%A, %e. %B", locale())
					}.to_string()
				}; 
				ui.clone().upgrade_in_event_loop(move |ui| {
					ui.set_datetime(ui::DateTime {
						date: new_date.into(),
						time: new_time.into(),
						seconds: now.second() as i32,
					});
				});
				time::sleep(time::Duration::from_secs(1)).await;
			};
	}});

	time_task.await?;
	Ok(())
}

fn error_showable(err: anyhow::Error) -> sixtyfps::SharedString {
	let cause: String = err.chain().skip(1).take(1)
		.map(|err| err.to_string()).collect::<String>()
		.split(": ").collect::<Vec<_>>()
		.join("\n");

	format!("{}\n{}", err, cause).into()
}

fn locale() -> chrono::Locale {
	static LOCALE_INIT: Once = Once::new();
	static mut LOCALE: chrono::Locale = chrono::Locale::POSIX;

	unsafe {
		LOCALE_INIT.call_once(|| {
			LOCALE = sys_locale::get_locale()
				.and_then(|locstr| chrono::Locale::try_from(locstr.as_str()).ok())
				.expect("failed to get system locale");
		});
		LOCALE
	}
}
