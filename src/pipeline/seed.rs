use crate::args::{read_query_format_from_mmseqs_query_db, FileFormat};
use crate::extension_traits::{CommandExt, PathBufExt};

use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::process::Command;

use nale::align::bounded::structs::Seed;
use nale::align::needleman_wunsch::{needleman_wunsch, SimpleTraceStep};
use nale::structs::hmm::parse_hmms_from_p7hmm_file;
use nale::structs::{Profile, Sequence};

use crate::pipeline::PrepPaths;
use anyhow::Context;
use clap::Args;
use thiserror::Error;

pub type SeedMap = HashMap<String, Vec<Seed>>;

#[derive(Args)]
pub struct MmseqsArgs {
    /// MMseqs2 prefilter: k-mer length (0: automatically set to optimum)
    #[arg(long = "mmseqs_k", default_value_t = 0usize)]
    k: usize,
    /// MMseqs2 prefilter: k-mer threshold for generating similar k-mer lists
    #[arg(long = "mmseqs_k_score", default_value_t = 80usize)]
    k_score: usize,
    /// MMseqs2 prefilter: Accept only matches with ungapped alignment score above threshold
    #[arg(long = "mmseqs_min_ungapped_score", default_value_t = 15usize)]
    min_ungapped_score: usize,
    /// MMseqs2 prefilter: Maximum results per query sequence allowed to pass the prefilter
    #[arg(long = "mmseqs_max_seqs", default_value_t = 1000usize)]
    max_seqs: usize,
    /// MMseqs2 align: Include matches below this E-value as seeds
    #[arg(long = "mmseqs_e", default_value_t = 1000f64)]
    e: f64,
}

#[derive(Args)]
pub struct SeedArgs {
    /// The location of files prepared with mmoreseqs prep
    #[arg()]
    prep_dir_path: PathBuf,
    /// Where to place the seeds output file
    #[arg(short, long, default_value = "seeds.json")]
    seeds_path: PathBuf,
    /// The path to a pre-built P7HMM file
    #[arg(long = "query_hmm", value_name = "QUERY.hmm")]
    prebuilt_query_hmm_path: Option<PathBuf>,
    /// The number of threads to use
    #[arg(short, long, default_value_t = 8usize, value_name = "n")]
    num_threads: usize,
    #[command(flatten)]
    mmseqs_args: MmseqsArgs,
}

impl PrepPaths for SeedArgs {
    fn prep_dir_path(&self) -> &PathBuf {
        &self.prep_dir_path
    }
}

pub fn seed(args: &SeedArgs) -> anyhow::Result<(Vec<Profile>, SeedMap)> {
    Command::new("mmseqs")
        .arg("prefilter")
        .arg(&args.mmseqs_query_db_path())
        .arg(&args.mmseqs_target_db_path())
        .arg(&args.mmseqs_prefilter_db_path())
        .args(["--threads", &args.num_threads.to_string()])
        .args(["-k", &args.mmseqs_args.k.to_string()])
        .args(["--k-score", &args.mmseqs_args.k_score.to_string()])
        .args([
            "--min-ungapped-score",
            &args.mmseqs_args.min_ungapped_score.to_string(),
        ])
        .args(["--max-seqs", &args.mmseqs_args.max_seqs.to_string()])
        .run()?;

    Command::new("mmseqs")
        .arg("align")
        .arg(&args.mmseqs_query_db_path())
        .arg(&args.mmseqs_target_db_path())
        .arg(&args.mmseqs_prefilter_db_path())
        .arg(&args.mmseqs_align_db_path())
        .args(["--threads", &args.num_threads.to_string()])
        .args(["-e", &args.mmseqs_args.e.to_string()])
        // the '-a' argument enables alignment backtraces in mmseqs2
        // it is required to get start positions for alignments
        .args(["-a", "1"])
        .run()?;

    Command::new("mmseqs")
        .arg("convertalis")
        .arg(&args.mmseqs_query_db_path())
        .arg(&args.mmseqs_target_db_path())
        .arg(&args.mmseqs_align_db_path())
        .arg(&args.mmseqs_align_tsv_path())
        .args(["--threads", &args.num_threads.to_string()])
        .args([
            "--format-output",
            "query,target,qstart,qend,tstart,tend,evalue",
        ])
        .run()?;

    let hmms = match &args.prebuilt_query_hmm_path {
        Some(prebuilt_hmm_path) => parse_hmms_from_p7hmm_file(prebuilt_hmm_path)?,
        _ => parse_hmms_from_p7hmm_file(args.prep_query_hmm_path())?,
    };

    let p7_profiles: Vec<Profile> = hmms.iter().map(Profile::new).collect();

    let profile_seeds_by_accession =
        build_alignment_seeds(&p7_profiles, args).context("failed to build alignment seeds")?;

    let mut seeds_out = args
        .seeds_path
        .open(true)
        .context("failed to create alignment seeds file")?;

    write!(
        seeds_out,
        "{}",
        serde_json::to_string(&profile_seeds_by_accession)?
    )
    .context("failed to write alignment seeds")?;

    Ok((p7_profiles, profile_seeds_by_accession))
}

pub fn map_p7_to_mmseqs_profiles(
    p7_profiles: &[Profile],
    args: &SeedArgs,
) -> anyhow::Result<HashMap<String, Vec<usize>>> {
    let mmseqs_consensus_map = extract_mmseqs_profile_consensus_sequences(args)?;

    let mut mmseqs_to_p7_idx_by_accession: HashMap<String, Vec<usize>> = HashMap::new();

    for p7_profile in p7_profiles {
        let accession = &p7_profile.accession;
        let mmseqs_consensus = mmseqs_consensus_map.get(accession).unwrap();
        let p7_consensus = Sequence::from_utf8(&p7_profile.consensus_sequence[1..])?;
        let trace = needleman_wunsch(mmseqs_consensus, &p7_consensus);

        let mut mmseqs_to_p7: Vec<usize> = vec![0; mmseqs_consensus.length + 1];

        let mut mmseqs_idx: usize = 0;
        let mut p7_idx: usize = 0;
        for step in &trace {
            match step {
                SimpleTraceStep::Diagonal => {
                    mmseqs_idx += 1;
                    p7_idx += 1;
                }
                SimpleTraceStep::Up => {
                    mmseqs_idx += 1;
                }
                SimpleTraceStep::Left => {
                    p7_idx += 1;
                }
            }
            mmseqs_to_p7[mmseqs_idx] = p7_idx;
        }

        // this debug assert should guarantee that the NW
        // alignment fully covered both consensus sequences
        debug_assert_eq!(mmseqs_idx, mmseqs_consensus.length);
        debug_assert_eq!(p7_idx, p7_consensus.length);

        mmseqs_to_p7_idx_by_accession.insert(accession.clone(), mmseqs_to_p7);
    }

    Ok(mmseqs_to_p7_idx_by_accession)
}

pub fn extract_mmseqs_profile_consensus_sequences(
    args: &SeedArgs,
) -> anyhow::Result<HashMap<String, Sequence>> {
    let mut offsets_and_lengths: Vec<(usize, usize)> = vec![];
    let mut accession_numbers: Vec<String> = vec![];

    let query_db_h_index_file = File::open(&args.mmseqs_query_db_h_index_path())
        .context("failed to open queryDB_h.index")?;

    let reader = BufReader::new(query_db_h_index_file);
    for line in reader.lines() {
        match line {
            Ok(line) => {
                let tokens: Vec<&str> = line.split_whitespace().collect();

                let offset = tokens[1].parse::<usize>()?;
                let length = tokens[2].parse::<usize>()?;
                offsets_and_lengths.push((offset, length));
            }
            Err(e) => {
                return Err(e).context("failed to parse line in queryDB_h.index");
            }
        }
    }

    let mut query_db_h_file =
        File::open(&args.mmseqs_query_db_h_path()).context("failed to open queryDB_h")?;

    for (offset, length) in &offsets_and_lengths {
        let mut buffer = vec![0; *length];
        query_db_h_file.seek(SeekFrom::Start(*offset as u64))?;
        query_db_h_file.read_exact(&mut buffer)?;

        let mut accession_string: Option<String> = None;
        for (buf_idx, byte) in buffer.iter().enumerate() {
            if byte.is_ascii_whitespace() {
                accession_string = Some(
                    std::str::from_utf8(&buffer[0..buf_idx])
                        .context("failed to create accession string")?
                        .to_string(),
                );
                break;
            }
        }

        match accession_string {
            Some(accession) => accession_numbers.push(accession),
            None => {
                panic!()
            }
        }
    }

    let query_db_index_file =
        File::open(&args.mmseqs_query_db_index_path()).context("failed to open queryDB.index")?;

    let reader = BufReader::new(query_db_index_file);
    for line in reader.lines() {
        match line {
            Ok(line) => {
                let tokens: Vec<&str> = line.split_whitespace().collect();

                let line_idx = tokens[0].parse::<usize>()?;
                let offset = tokens[1].parse::<usize>()?;
                let length = tokens[2].parse::<usize>()?;
                offsets_and_lengths[line_idx] = (offset, length);
            }
            Err(e) => {
                return Err(e).context("failed to parse line in queryDB.index");
            }
        }
    }

    let mut sequence_map: HashMap<String, Sequence> = HashMap::new();

    let mut query_db_file =
        File::open(&args.mmseqs_query_db_path()).context("failed to open queryDB")?;

    for (seq_idx, (offset, length)) in offsets_and_lengths.iter().enumerate() {
        let mut buffer = vec![0; *length];
        query_db_file.seek(SeekFrom::Start(*offset as u64))?;
        query_db_file.read_exact(&mut buffer)?;

        let mut consensus_digital_bytes: Vec<u8> = vec![];

        for byte_chunk in buffer.chunks(23) {
            if byte_chunk.len() == 23 {
                consensus_digital_bytes.push(byte_chunk[21]);
            }
        }

        sequence_map.insert(
            accession_numbers[seq_idx].clone(),
            Sequence::from_digital(&consensus_digital_bytes)?,
        );
    }

    Ok(sequence_map)
}

#[derive(Error, Debug)]
#[error("no profile to profile map for: {profile_name}")]
pub struct ProfilesNotMappedError {
    pub profile_name: String,
}

#[derive(Error, Debug)]
#[error("no profile name for accession: {accession}")]
pub struct AccessionNotMappedError {
    pub accession: String,
}

// TODO: move this elsewhere
#[derive(Error, Debug)]
#[error("invalid file format: {format}")]
pub struct InvalidFileFormatError {
    pub format: FileFormat,
}

pub fn build_alignment_seeds(
    p7_profiles: &Vec<Profile>,
    args: &SeedArgs,
) -> anyhow::Result<SeedMap> {
    let mut accession_to_name: HashMap<&str, &str> = HashMap::new();

    for profile in p7_profiles {
        accession_to_name.insert(&profile.accession, &profile.name);
    }

    let mut seed_map: SeedMap = HashMap::new();

    let mmseqs_align_file = File::open(&args.mmseqs_align_tsv_path()).context(format!(
        "couldn't open mmseqs align file at: {}",
        &args.mmseqs_align_tsv_path().to_string_lossy()
    ))?;

    let align_reader = BufReader::new(mmseqs_align_file);

    let query_format = read_query_format_from_mmseqs_query_db(&args.mmseqs_query_dbtype_path())?;

    let profile_to_profile_idx_maps_by_accession = match query_format {
        // if the query was a fasta, we don't need to map between
        // profiles (because we don't actually have profiles)
        FileFormat::Fasta => None,
        // if the query was a stockholm, then it was used to build
        // both a P7 HMM and an MMseqs2 profile, which consistently
        // have significant differences in consensus columns
        FileFormat::Stockholm => {
            Some(map_p7_to_mmseqs_profiles(p7_profiles, args).context("failed to map profiles")?)
        }
        ref format => {
            return Err(InvalidFileFormatError {
                format: format.clone(),
            }
            .into())
        }
    };

    for line in align_reader.lines().flatten() {
        let line_tokens: Vec<&str> = line.split_whitespace().collect();
        let target_name = line_tokens[1].to_string();
        let target_start = line_tokens[4].parse::<usize>()?;
        let target_end = line_tokens[5].parse::<usize>()?;
        let mut profile_start = line_tokens[2].parse::<usize>()?;
        let mut profile_end = line_tokens[3].parse::<usize>()?;

        let profile_name = match query_format {
            FileFormat::Fasta => line_tokens[0].to_string(),
            FileFormat::Stockholm => {
                let accession = line_tokens[0];

                let profile_name =
                    (*accession_to_name
                        .get(accession)
                        .ok_or(AccessionNotMappedError {
                            accession: accession.to_string(),
                        })?)
                    .to_string();

                if let Some(ref map) = profile_to_profile_idx_maps_by_accession {
                    let profile_idx_map = map.get(accession).ok_or(ProfilesNotMappedError {
                        profile_name: profile_name.clone(),
                    })?;
                    // if the profile index map happens to map the start to 0, we want to push it to 1
                    profile_start = profile_idx_map[profile_start].max(1);
                    profile_end = profile_idx_map[profile_end];
                }
                profile_name
            }
            _ => {
                panic!()
            }
        };

        let seeds = match seed_map.get_mut(&profile_name) {
            Some(seeds) => seeds,
            None => {
                seed_map.insert(profile_name.clone(), vec![]);
                seed_map.get_mut(&profile_name).unwrap()
            }
        };

        seeds.push(Seed {
            target_name,
            target_start,
            target_end,
            profile_start,
            profile_end,
        })
    }
    Ok(seed_map)
}
