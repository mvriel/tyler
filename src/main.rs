mod cli;
mod formats;
mod parser;
mod proj;
mod spatial_structs;

use std::collections::HashMap;
use std::fs;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

use clap::{crate_version, Arg, ArgAction, Command, Parser};
use log::{debug, error, info, log_enabled, Level};
use parser::FeatureSet;
use rayon::prelude::*;
use subprocess::{Exec, Redirection};
use walkdir::WalkDir;

#[derive(Debug, Default, Clone)]
struct SubprocessConfig {
    output_extension: String,
    exe: PathBuf,
    script: PathBuf,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();

    // --- Begin argument parsing
    let cli = crate::cli::Cli::parse();
    if !cli.output.is_dir() {
        fs::create_dir_all(&cli.output)?;
        info!("Created output directory {:#?}", &cli.output);
    }
    // Since we have a default value, we can safely unwrap.
    let grid_cellsize = cli.grid_cellsize.unwrap();
    let subprocess_config = match cli.format.as_str() {
        "3dtiles" => {
            if let Some(exe) = cli.exe_geof {
                SubprocessConfig {
                    output_extension: "glb".to_string(),
                    exe,
                    script: PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                        .join("resources")
                        .join("geof")
                        .join("createGLB.json"),
                }
            } else {
                panic!("exe_geof must be set for generating 3D Tiles");
            }
        }
        "cityjson" => {
            if let Some(exe) = cli.exe_python {
                SubprocessConfig {
                    output_extension: "city.json".to_string(),
                    exe,
                    script: PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                        .join("resources")
                        .join("python")
                        .join("convert_cityjsonfeatures.py"),
                }
            } else {
                panic!("exe_python must be set for generating CityJSON tiles")
            }
        }
        _ => SubprocessConfig::default(),
    };
    debug!("{:?}", &subprocess_config);
    // Since we have a default value, it is safe to unwrap
    let quadtree_capacity = match &cli.qtree_capacity_type.unwrap() {
        spatial_structs::QuadTreeCapacityType::Objects => {
            spatial_structs::QuadTreeCapacity::Objects(cli.qtree_capacity.unwrap())
        }
        spatial_structs::QuadTreeCapacityType::Vertices => {
            spatial_structs::QuadTreeCapacity::Vertices(cli.qtree_capacity.unwrap())
        }
    };
    // --- end of argument parsing

    // Populate the World with features
    // Primitive types that implement Copy are efficiently copied into the function and
    // and it is cleaner to avoid the indirection. However, heap-allocated container
    // types are best passed by reference, because it is "expensive" to Clone them
    // (they don't implement Copy). When we move a value, we explicitly transfer
    // ownership of the value (eg cli.object_type).
    let mut world = parser::World::new(
        &cli.metadata,
        &cli.features,
        grid_cellsize,
        cli.object_type,
        cli.grid_minz,
        cli.grid_maxz,
    )?;
    world.index_with_grid();

    // Debug
    if cli.grid_export {
        debug!("Exporting the grid to the working directory");
        world.export_grid()?;
    }

    // Build quadtree
    info!("Building quadtree");
    let quadtree = spatial_structs::QuadTree::from_world(&world, quadtree_capacity);

    // 3D Tiles
    info!("Generating 3D Tiles tileset");
    let tileset_path = cli.output.join("tileset.json");
    let tileset = formats::cesium3dtiles::Tileset::from_quadtree(
        &quadtree,
        &world,
        cli.grid_minz,
        cli.grid_maxz,
    );
    tileset.to_file(tileset_path)?;

    // Export by calling a subprocess to merge the .jsonl files and convert them to the
    // target format
    let path_output_tiles = cli.output.join("tiles");
    if !path_output_tiles.is_dir() {
        fs::create_dir_all(&path_output_tiles)?;
        info!("Created output directory {:#?}", &path_output_tiles);
    }
    let path_features_input_dir = cli.output.join("inputs");
    if !path_features_input_dir.is_dir() {
        fs::create_dir_all(&path_features_input_dir)?;
        info!("Created output directory {:#?}", &path_features_input_dir);
    }
    let cotypes_str: Vec<String> = match &world.cityobject_types {
        None => Vec::new(),
        Some(cotypes) => cotypes.iter().map(|co| co.to_string()).collect(),
    };
    let cotypes_arg = cotypes_str.join(",");

    let leaves: Vec<&spatial_structs::QuadTree> = quadtree.collect_leaves();
    info!("Exporting and optimizing {} tiles", leaves.len());
    if &cli.format == "3dtiles" && cli.exe_gltfpack.is_none() {
        debug!("exe_gltfpack is not set, skipping gltf optimization")
    };
    leaves.into_par_iter().for_each(|tile| {
        if tile.nr_items > 0 {
            let tileid = tile.id();
            let file_name = format!("{}", &tileid);
            let output_file = path_output_tiles
                .join(&file_name)
                .with_extension(&subprocess_config.output_extension);
            // We write the list of feature paths for a tile into a text file, instead of passing
            // super long paths-string to the subprocess, because with very long arguments we can
            // get an 'Argument list too long' error.
            let path_features_input_file = path_features_input_dir
                .join(&file_name)
                .with_extension("input");
            fs::create_dir_all(path_features_input_file.parent().unwrap()).unwrap_or_else(|_| {
                panic!(
                    "should be able to create the directory {:?}",
                    path_features_input_file.parent().unwrap()
                )
            });
            let mut feature_input = File::create(&path_features_input_file).unwrap_or_else(|_| {
                panic!(
                    "should be able to create a file {:?}",
                    &path_features_input_file
                )
            });
            for cellid in tile.cells.iter() {
                let cell = world.grid.cell(cellid);
                for fid in cell.feature_ids.iter() {
                    let fp = world.features[*fid]
                        .path_jsonl
                        .clone()
                        .into_os_string()
                        .into_string()
                        .unwrap();
                    writeln!(feature_input, "{}", fp)
                        .expect("should be able to write feature path to the input file");
                }
            }

            let b = tile.bbox(&world.grid);
            // We need to string-format all the arguments with an = separator, because that's what
            // geof can accept.
            // TODO: maybe replace the subprocess carte with std::process to remove the dependency
            let mut cmd = Exec::cmd(&subprocess_config.exe)
                .arg(&subprocess_config.script)
                .arg(format!("--output_format={}", &cli.format))
                .arg(format!("--output_file={}", &output_file.to_str().unwrap()))
                .arg(format!(
                    "--path_metadata={}",
                    &world.path_metadata.to_str().unwrap()
                ))
                .arg(format!(
                    "--path_features_input_file={}",
                    &path_features_input_file.to_str().unwrap()
                ))
                .arg(format!("--min_x={}", b[0]))
                .arg(format!("--min_y={}", b[1]))
                .arg(format!("--min_z={}", b[2]))
                .arg(format!("--max_x={}", b[3]))
                .arg(format!("--max_y={}", b[4]))
                .arg(format!("--max_z={}", b[5]))
                .arg(format!("--cotypes={}", &cotypes_arg));

            if &cli.format == "3dtiles" {
                // geof specific args
                if let Some(ref cotypes) = world.cityobject_types {
                    if cotypes.contains(&parser::CityObjectType::Building)
                        || cotypes.contains(&parser::CityObjectType::BuildingPart)
                    {
                        cmd = cmd.arg("--simplify_ratio=1.0").arg("--skip_clip=true");
                    }
                }
                if log_enabled!(Level::Debug) {
                    cmd = cmd.arg("--verbose");
                }
            }
            debug!("{}", cmd.to_cmdline_lossy());
            let res_exit_status = cmd
                .stdout(Redirection::Pipe)
                .stderr(Redirection::Merge)
                .capture();
            if let Ok(capturedata) = res_exit_status {
                let stdout = capturedata.stdout_str();
                if !capturedata.success() {
                    error!("{} conversion subprocess stdout: {}", &tileid, stdout);
                    error!(
                        "{} conversion subprocess stderr: {}",
                        &tileid,
                        capturedata.stderr_str()
                    );
                } else if !stdout.is_empty() && stdout != "\n" {
                    debug!(
                        "{} conversion subproces stdout {}",
                        &tileid,
                        capturedata.stdout_str()
                    );
                }
            } else if let Err(popen_error) = res_exit_status {
                error!("{}", popen_error);
            }
            // Run gltfpack on the produced glb
            if cli.format.as_str() == "3dtiles" {
                if let Some(ref gltfpack) = cli.exe_gltfpack {
                    let res_exit_status = Exec::cmd(gltfpack)
                        .arg("-cc")
                        .arg("-kn")
                        .arg("-i")
                        .arg(&output_file)
                        .arg("-o")
                        .arg(&output_file)
                        .stdout(Redirection::Pipe)
                        .stderr(Redirection::Merge)
                        .capture();
                    if let Ok(capturedata) = res_exit_status {
                        let stdout = capturedata.stdout_str();
                        if !capturedata.success() {
                            error!("{} gltfpack subprocess stdout: {}", &tileid, stdout);
                            error!(
                                "{} gltfpack subprocess stderr: {}",
                                &tileid,
                                capturedata.stderr_str()
                            );
                        } else if !stdout.is_empty() && stdout != "\n" {
                            debug!(
                                "{} gltfpack subproces stdout {}",
                                &tileid,
                                capturedata.stdout_str()
                            );
                        }
                    } else if let Err(popen_error) = res_exit_status {
                        error!("{}", popen_error);
                    }
                }
            }
        } else {
            debug!("tile {} is empty", &tile.id())
        }
    });
    info!("Done");
    debug!("Deleting {:?}", &path_features_input_dir);
    fs::remove_dir_all(path_features_input_dir)?;
    Ok(())
}
