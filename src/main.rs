use anyhow::{anyhow, Result};
use audiotags;
use clap::Parser;
use homedir;
use edit_distance;
use reqwest::{self, header};
use serde_json;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use thiserror::Error;
use tokio;
use urlencoding;
use walkdir::WalkDir;

/// Simple program to greet a person
#[derive(Parser, Debug)]
struct Args {
    file: PathBuf,

    /// Whether to recursively search a directory
    #[arg(short, long)]
    recursive: bool,

    /// Filename for image
    #[arg(short, long, default_value = "cover.jpg")]
    output: PathBuf,

    /// Force overwriting the existing output file
    #[arg(short, long)]
    force: bool,
}

async fn get_access_token(client_id: &str, client_secret: &str) -> Result<String> {
    let client = reqwest::Client::new();
    let response = client
        .post("https://accounts.spotify.com/api/token")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(format!(
            "grant_type=client_credentials&client_id={client_id}&client_secret={client_secret}"
        ))
        .send()
        .await?;

    let content = response.text().await?;
    let json_object: serde_json::Value = serde_json::from_str(&content)?;
    let access_token = json_object["access_token"]
        .as_str()
        .ok_or(anyhow!("Error: invalid field in response: `access_token`"))?
        .to_string();
    Ok(access_token)
}

async fn search(
    access_token: &str,
    track_name: &str,
    artist_names: &Vec<&str>,
) -> Result<serde_json::Value> {
    let track_name_encoded = urlencoding::encode(&track_name);
    let client = reqwest::Client::new();
    let response = client
        .get(format!(
            "https://api.spotify.com/v1/search?q=track%3A{track_name_encoded}%20artist%3A{artist}&type=track",
            artist = artist_names[0],

        ))
        .header("Accept", "application/json")
        .header("User-Agent", "Rust")
        .header(header::AUTHORIZATION, format!("Bearer {access_token}"))
        .send()
        .await?;
    let content = response.text().await?;

    Ok(serde_json::from_str(&content)?)
}

fn calculate_average_artist_names_distance(a: &Vec<&str>, b: &Vec<&str>) -> usize {
    let num_artists = a.len();
    let num_found_artists = b.len();

    let (larger, smaller) = if num_artists > num_found_artists {
        (a, b)
    } else {
        (b, a)
    };

    let mut total_distance = 0usize;
    for outer_artist_name in smaller.iter() {
        let mut min_distance: Option<usize> = None;
        for inner_artist_name in larger.iter() {
            let distance = edit_distance::edit_distance(outer_artist_name, inner_artist_name);
            min_distance = match min_distance {
                Some(min_distance) => Some(min_distance.min(distance)),
                None => Some(distance),
            };
        }
        total_distance += min_distance.expect("There should be at least one artist for the track");
    }

    total_distance / num_found_artists
}

async fn get_image_url_for_track(
    access_token: &str,
    track_name: &str,
    artist_names: &Vec<&str>,
    album_name: &str,
) -> Result<String> {
    let res = search(&access_token, track_name, artist_names).await?;

    let mut tracks = res["tracks"]["items"]
        .as_array()
        .ok_or(anyhow!("Results should be an array"))?
        .to_owned();
    tracks.sort_by_key(|found_track| {
        let found_track_name = found_track["name"]
            .as_str()
            .expect("Track name should be a string");
        let found_track_artist_names: Vec<_> = found_track["artists"]
            .as_array()
            .expect("Track artists should be an array")
            .iter()
            .map(|artist| {
                artist["name"]
                    .as_str()
                    .expect("Artist name should be a string")
            })
            .collect();
        let found_track_album_name = found_track["album"]["name"]
            .as_str()
            .expect("Album name should be a string");

        let track_name_distance = edit_distance::edit_distance(track_name, found_track_name);
        let artist_name_distance = calculate_average_artist_names_distance(artist_names, &found_track_artist_names);
        let album_name_disatnce = edit_distance::edit_distance(album_name, found_track_album_name);

        track_name_distance + artist_name_distance + album_name_disatnce
    });

    let track = if tracks.len() <= 1 {
        &tracks[0]
    } else {
        let mut to_return: Option<&serde_json::Value> = None;
        for track in tracks.iter() {
            if track["album"]["name"] == serde_json::Value::String(album_name.to_string()) {
                to_return = Some(track);
                break;
            }
        }
        match to_return {
            Some(track) => track,
            None => &tracks[0],
        }
    };

    let images = track["album"]["images"]
        .as_array()
        .ok_or(anyhow!("Invalid images array"))?;
    let image_url = images[0]["url"]
        .as_str()
        .ok_or(anyhow!("Invalid image url"))?;

    Ok(image_url.to_string())
}

#[derive(Error, Debug)]
#[error("Invalid Filetype")]
pub struct InvalidFiletype;

async fn get_image_url_from_filename(
    access_token: &str,
    filename: impl AsRef<Path>,
) -> Result<String> {
    let tag = match audiotags::Tag::new().read_from_path(filename) {
        Ok(tag) => tag,
        Err(_) => return Err(anyhow::Error::new(InvalidFiletype)),
    };
    let track_name = tag.title().ok_or(anyhow!("Invalid song title"))?;
    let artist_names: Vec<_> = tag
        .artist()
        .ok_or(anyhow!("Invalid song artists"))?
        .split(", ")
        .collect();
    let album_name = tag
        .album_title()
        .ok_or(anyhow!("Invalid song album name"))?;

    let image_url =
        get_image_url_for_track(&access_token, track_name, &artist_names, album_name).await?;
    return Ok(image_url);
}

fn log(msg: impl AsRef<str>) {
    println!("SPOT_IMG_SEARCH: {}", msg.as_ref());
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let config_home = homedir::my_home()?.unwrap().join(".config/spotify-image-search");
    let client_id_file = config_home.join("client_id");
    let client_secret_file = config_home.join("client_secret");

    let client_id = fs::read_to_string(client_id_file)?.trim().to_string();
    let client_secret = fs::read_to_string(client_secret_file)?.trim().to_string();
    let access_token = get_access_token(&client_id, &client_secret).await?;

    if args.file.is_dir() {
        if args.recursive {
            for entry in WalkDir::new(&args.file) {
                let filepath = entry.unwrap().path().to_path_buf();
                let image_file_path = filepath.parent().unwrap().join(&args.output);
                if !args.force {
                    if image_file_path.exists() {
                        continue;
                    }
                }
                if !filepath.is_dir() {
                    log("Searching for image...");
                    match get_image_url_from_filename(&access_token, &filepath).await {
                        Ok(image_url) => {
                            log(format!("Found image: {}", image_url));
                            let image_data = reqwest::get(image_url).await?.bytes().await?;

                            let mut image_file = fs::File::create(&image_file_path)?;
                            log(format!("Writing to file: {}", image_file_path.into_os_string().into_string().unwrap()));
                            image_file.write_all(&image_data)?;
                        }
                        Err(_) => {
                            continue;
                        }
                    }
                }
            }
        } else {
            return Err(anyhow!(
                "Cannot provide directory unless --recursive,-r is specified"
            ));
        }
    } else {
        let image_file_path = &args.file.parent().unwrap().join(&args.output);
        log("Searching for image...");
        let image_url = get_image_url_from_filename(&access_token, &args.file).await?;
        log(format!("Found image: {}", image_url));
        let image_data = reqwest::get(image_url).await?.bytes().await?;
        let mut image_file = if args.force {
            fs::File::create(&image_file_path)?
        } else {
            fs::File::create_new(&image_file_path)?
        };
        log(format!("Writing to file: {}", image_file_path.clone().into_os_string().into_string().unwrap()));
        image_file.write_all(&image_data)?;
    };

    Ok(())
}

#[cfg(test)]
mod test {
    use anyhow::Result;
    use walkdir::WalkDir;

    #[test]
    fn walk_dir() -> Result<()> {
        for entry in WalkDir::new("/home/composer3/Music") {
            println!("{}", entry?.path().display());
        }
        Ok(())
    }
}
