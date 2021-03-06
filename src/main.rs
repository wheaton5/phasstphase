#[macro_use]
extern crate clap;
extern crate bio;
extern crate hashbrown;
extern crate rand;
extern crate rayon;
extern crate threadpool;
extern crate statrs;
extern crate petgraph;
extern crate sanitize_filename;

use rand::rngs::StdRng;
use rand::Rng;
use rand::SeedableRng;
use std::process::Command;
use std::{thread, time};
use threadpool::ThreadPool;
use std::sync::{Arc, Barrier};

use statrs::distribution::{Binomial};
use statrs::distribution::Discrete;
use bio::alignment::pairwise::banded;
use bio::io::fasta;
use bio::utils::TextSlice;
use petgraph::unionfind::UnionFind;

use statrs::distribution::Multinomial;

use failure::{Error, ResultExt};
use flate2::read::MultiGzDecoder;
use rayon::prelude::*;
use rust_htslib::bam::ext::BamRecordExtensions;
use rust_htslib::bam::{self, Read, Record};
use rust_htslib::bcf::record::GenotypeAllele;
use rust_htslib::bcf::Format;
use rust_htslib::bcf::{self, Read as BcfRead};
use std::convert::TryInto;
use std::fs;
use std::fs::File;
use std::io::Write;
use std::path::Path;
use crate::statrs::distribution::DiscreteCDF;

use hashbrown::{HashMap, HashSet};

use clap::App;

const K: usize = 6; // kmer match length
const W: usize = 20; // Window size for creating the band
const MATCH: i32 = 1; // Match score
const MISMATCH: i32 = -5; // Mismatch score
const GAP_OPEN: i32 = -5; // Gap open score
const GAP_EXTEND: i32 = -1; // Gap extend score

fn main() {
    let result = _main();
    if let Err(v) = result {
        let fail = v.as_fail();
        println!("Phasstphase error. v{}.", env!("CARGO_PKG_VERSION"));
        println!("Error: {}", fail);

        for cause in fail.iter_causes() {
            println!("Info: caused by {}", cause);
        }

        println!("\n{}", v.backtrace());

        println!("If you think this is bug in Phasstphase, please file a bug at https://github.com/wheaton5/phasstphase, and include the information above and the command-line you used.");
        std::process::exit(1)
    }
}

fn _main() -> Result<(), Error> {
    println!("Welcome to phasst phase!");
    let params = load_params();
    //if params.long_read_bam == None && params.linked_read_bam == None && params.hic_bam == None {
    //    eprintln!("Must supply at least one bam");
    //    std::process::exit(1);
    //}
    if Path::new(&params.output).is_dir() {
        eprintln!("restarting from partial output in {}", params.output);
    } else {
        fs::create_dir(params.output.to_string())?;
    }

    let fai = params.fasta.to_string() + ".fai";
    let fa_index_iter = fasta::Index::from_file(&fai)
        .expect(&format!("error opening fasta index: {}", fai))
        .sequences();
    let mut chroms: Vec<String> = Vec::new();
    //chroms.push("chr20".to_string());
    let mut chrom_lengths: Vec<u64> = Vec::new();
    //chrom_lengths.push(63025520);
    for chrom in fa_index_iter {
        chroms.push(chrom.name.to_string());
        chrom_lengths.push(chrom.len);
    }
    
    //let vcf_reader = bcf::IndexedReader::from_path(params.vcf.to_string())?;
    let mut chunks: Vec<ThreadData> = Vec::new();
    for (i, chrom) in chroms.iter().enumerate() {
        //if chrom.chars().count() > 5 { continue; }
        println!("chrom {}end",chrom);
        let data = ThreadData {
            index: i,
            long_read_bam: match &params.long_read_bam {
                Some(x) => Some(x.to_string()),
                None => None,
            },
            hic_bam: match &params.hic_bam {
                Some(x) => Some(x.to_string()),
                None => None,
            },
            fasta: params.fasta.to_string(),
            chrom: chrom.to_string(),
            vcf: params.vcf.to_string(),
            min_mapq: params.min_mapq,
            min_base_qual: params.min_base_qual,
            chrom_length: chrom_lengths[i],
            window: params.window,
            output: params.output.to_string(),
            vcf_out: format!("{}/chrom_{}.vcf.gz", params.output, sanitize_filename::sanitize(chrom)),
            vcf_out_done: format!("{}/chrom_{}.vcf.done", params.output, sanitize_filename::sanitize(chrom)),
            phasing_window: params.phasing_window,
            seed: params.seed,
            ploidy: params.ploidy,
            hic_phasing_posterior_threshold: params.hic_phasing_posterior_threshold,
            long_switch_threshold: params.long_switch_threshold,
        };
        chunks.push(data);
    }
    //let pool = rayon::ThreadPoolBuilder::new()
    //    .num_threads(params.threads)
    //    .build()
    //    .unwrap();
    //let results: Vec<_> = pool.install(|| {
    //    chunks
    //        .par_iter()
    //        .map(|rec_chunk| phase_chunk(&rec_chunk))
    //        .collect()
    //});
    //for data in chunks {
    //   pool.spawn(move || phase_chunk(&data).expect("thread failed"));
    //}
    let pool = ThreadPool::new(params.threads);
    let barrier = Arc::new(Barrier::new(params.threads + 1));
    let final_output = format!("{}/phasstphase.vcf.gz",params.output);
    let mut cmd: Vec<String> = Vec::new();
    cmd.push("concat".to_string());
    cmd.push("-o".to_string());
    cmd.push(final_output.to_string());
    
    for data in chunks {
        cmd.push(format!("{}/phased_chrom_{}.vcf.gz", data.output, data.chrom));
        pool.execute(move|| phase_chunk(&data).expect("thread failed"));
    }
    barrier.wait();
   
    let result = Command::new("bcftools")
        .args(&cmd)
        .status()
        .expect("bcftools failed us");
    let result = Command::new("tabix")
        .args(&["-p", "vcf", &final_output])
        .status()
        .expect("couldn't tabix index final vcf");
    Ok(())
}

struct PhaseBlock {
    start_index: usize,
    start_position: usize,
    end_index: usize,
    end_position: usize,
}

fn phase_chunk(data: &ThreadData) -> Result<(), Error> {
    println!("thread {} chrom {}end", data.index, data.chrom);

    if !Path::new(&data.vcf_out_done).exists() {
        //println!("yeah, we are getting all variant assignments in thread {} chrom {}", data.index, data.chrom);
        get_all_variant_assignments(data).expect("we error here at get all variant assignments");
    }
    println!("gothere {} chrom{}after", data.index, data.chrom);
    let mut vcf_reader = bcf::IndexedReader::from_path(format!("{}", data.vcf_out.to_string()))
        .expect("could not open indexed vcf reader on output vcf");
    let chrom = vcf_reader
        .header()
        .name2rid(data.chrom.as_bytes())
        .expect("can't get chrom rid, make sure vcf and bam and fasta contigs match!");
    let vcf_info = inspect_vcf(&mut vcf_reader, &data);
    let mut cluster_centers = init_cluster_centers(vcf_info.num_variants, &data);
    let debug_bump = 0;
    let mut window_start: usize = debug_bump;
    let mut window_end: usize = window_start + data.phasing_window;
    //let mut position_to_index: HashMap<usize, usize> = HashMap::new();
    //let mut position_so_far: usize = 0;
    let mut phase_blocks: Vec<PhaseBlock> = Vec::new();
    let mut phase_block_start: usize = 0;
    let mut last_attempted_index: usize = 0;
    let mut in_phaseblock = false;
    eprintln!("{}", vcf_info.final_position);
    'outer: while (window_start as u64) < data.chrom_length
        && window_start < vcf_info.final_position as usize
    {
        let mut cluster_center_delta: f32 = 10.0;
        vcf_reader
            .fetch(chrom, window_start as u64, Some(window_end as u64))
            .expect("some actual error");
        //println!(
        //    "fetching region {}:{}-{}",
        //    data.chrom, window_start, window_end
        //);
        let (molecules, first_var_index, last_var_index) = get_read_molecules(&mut vcf_reader, &vcf_info, READ_TYPE::HIFI);
        //println!("{} molecules", molecules.len());
        let mut iteration = 0;
        let mut min_index: usize = 0;
        let mut max_index: usize = 0;

        while cluster_center_delta > 0.01 {
            let (breaking_point, posteriors, _log_likelihood) = expectation(&molecules, &cluster_centers);
            if in_phaseblock && breaking_point {
                //println!(
                //    "BREAKING due to no posteriors differing... window {}-{}",
                //    window_start, window_end
                //);
                in_phaseblock = false;
                while cluster_centers[0][last_attempted_index] == 0.5 {
                    last_attempted_index -= 1;
                }
                
                let cut_blocks = test_long_switch(phase_block_start, last_attempted_index, &mut cluster_centers, &vcf_info, &mut vcf_reader, &data);
                for phase_block in cut_blocks {
                    phase_blocks.push(phase_block);
                }
                //phase_blocks.push(PhaseBlock {
                //    start_index: phase_block_start,
                //    start_position: vcf_info.variant_positions[phase_block_start],
                //    end_index: last_attempted_index,
                //    end_position: vcf_info.variant_positions[last_attempted_index],
                //});
                println!(
                    "PHASE BLOCK ENDING {}-{}, {}-{} length {}",
                    phase_block_start,
                    last_attempted_index,
                    vcf_info.variant_positions[phase_block_start],
                    vcf_info.variant_positions[last_attempted_index],
                    vcf_info.variant_positions[last_attempted_index] - vcf_info.variant_positions[phase_block_start]
                );
                phase_block_start = last_attempted_index + 1;
                window_start = vcf_info.variant_positions[phase_block_start];
                //eprintln!("reseting window start to {}", window_start);
                window_end = window_start + data.phasing_window;
                let seed = [data.seed; 32];
                let mut rng: StdRng = SeedableRng::from_seed(seed);
                for haplotype in 0..cluster_centers.len() {
                    cluster_centers[haplotype][phase_block_start] =
                        rng.gen::<f32>().min(0.98).max(0.02);
                }
                
                continue 'outer;
            } if !in_phaseblock && breaking_point {
                if first_var_index == usize::MAX { // there were no variants
                    // we need to find a new starting point
                    
                    for (index, position) in vcf_info.variant_positions.iter().enumerate() {
                        phase_block_start = index;
                        if *position > window_end {
                            break;
                        }
                    }
                    //eprintln!("unable to start phaseblock, resetting");
                } else {
                    //eprintln!("unphased variants {}-{} positions {}-{}", first_var_index, last_var_index,
                    //vcf_info.variant_positions[first_var_index], vcf_info.variant_positions[last_var_index]);
                    phase_block_start = last_var_index + 1;
                }
                
                window_start = vcf_info.variant_positions[phase_block_start];
                //eprintln!("reseting window start to {}", window_start);
                window_end = window_start + data.phasing_window;
                let seed = [data.seed; 32];
                let mut rng: StdRng = SeedableRng::from_seed(seed);
                for haplotype in 0..cluster_centers.len() {
                    cluster_centers[haplotype][phase_block_start] =
                        rng.gen::<f32>().min(0.98).max(0.02);
                }
                continue 'outer;
            }
            cluster_center_delta = maximization(
                &molecules,
                posteriors,
                &mut cluster_centers,
                &mut min_index,
                &mut max_index,
            );
            if max_index != 0 {
                last_attempted_index = max_index;
                if !in_phaseblock {
                    phase_block_start = min_index;
                }
                in_phaseblock = true;
            }

            iteration += 1;
        }
        //println!(
        //    "converged in {} iterations, min {} max {}",
        //    iteration, min_index, max_index
        //);
        /*
        for haplotype in 0..cluster_centers.len() {
            print!("haplotype {}\t", haplotype);
            for variant in min_index..max_index {
                print!(
                    "var{},{}:{} | ",
                    variant,
                    vcf_info.variant_positions[variant],
                    cluster_centers[haplotype][variant]
                );
            }
            println!();
        }
        */
        window_start += data.phasing_window / 4;
        window_start = window_start.min(vcf_info.final_position as usize);
        //eprintln!("moving window start to {}", window_start);
        window_end += data.phasing_window / 4;
        window_end = window_end.min(vcf_info.final_position as usize);
        //break;
    }

    let cut_blocks = test_long_switch(phase_block_start, last_attempted_index, &mut cluster_centers, &vcf_info, &mut vcf_reader, &data);
    for phase_block in cut_blocks {
        phase_blocks.push(phase_block);
    }
    //phase_blocks.push(PhaseBlock {
    //    start_index: phase_block_start,
    //    start_position: vcf_info.variant_positions[phase_block_start],
    //    end_index: last_attempted_index,
    //    end_position: vcf_info.variant_positions[last_attempted_index],
    //});

    println!("DONE!");
    let mut total_gap_length = 0;
    for (id, phase_block) in phase_blocks.iter().enumerate() {
        let mut gap = phase_block.start_position;
        if id > 0 {
            gap = phase_block.start_position - phase_blocks[id - 1].end_position;
        }
        total_gap_length += gap;
        println!(
            "phase block {} from {}-{}, {}-{} length {} with gap from last of {}",
            id,
            phase_block.start_position,
            phase_block.end_position,
            phase_block.start_index,
            phase_block.end_index,
            phase_block.end_position - phase_block.start_position,
            gap
        );
    }
    if phase_blocks.len() > 0 {
        println!("and final gap of {}", (data.chrom_length as usize) - phase_blocks[phase_blocks.len() - 1].end_position);
    }
    
    println!("with total gap length of {}", total_gap_length);
    // get phaseblock N50... 
    let mut sizes: Vec<usize> = Vec::new();
    let mut total: usize = 0;
    for phase_block in phase_blocks.iter() {
        let length = phase_block.end_position - phase_block.start_position;
        sizes.push(length);
        total += length;
    }
    sizes.sort_by(|a, b| b.cmp(a));
    let mut so_far = 0;
    for size in sizes {
        so_far += size;
        if so_far > total/2 {
            println!("N50 phase blocks from long reads for chrom {} is {}", data.chrom, size);
            break;
        }
    }

    let new_phase_blocks = phase_phaseblocks(data, &mut cluster_centers, &phase_blocks);
    output_phased_vcf(data, cluster_centers, phase_blocks);
    println!("thread {} chrom {}finished", data.index, data.chrom);
    Ok(())
}

fn test_long_switch(start_index: usize, end_index: usize, cluster_centers: &mut Vec<Vec<f32>>, vcf_info: &VCF_info, vcf_reader: &mut bcf::IndexedReader, data: &ThreadData) -> Vec<PhaseBlock> {
    let mut to_return: Vec<PhaseBlock> = Vec::new();
    /*to_return.push(PhaseBlock{
        start_index: start_index,
        start_position: vcf_info.variant_positions[start_index],
        end_index: end_index,
        end_position: vcf_info.variant_positions[end_index],
    });
    return to_return; /// TODODODODODOD short circuiting for debug only
    */



    let pairings = pairings(data.ploidy);
    let log_prior = (1.0/(pairings.len() as f32)).ln();
    let chrom = vcf_reader
        .header()
        .name2rid(data.chrom.as_bytes())
        .expect("can't get chrom rid, make sure vcf and bam and fasta contigs match!");
    let mut phase_block_start = start_index;


    for breakpoint in start_index..end_index {
        let mut log_likelihoods: Vec<f32> = Vec::new();
        let position = vcf_info.variant_positions[breakpoint];
        vcf_reader
            .fetch(chrom, (position as u64).checked_sub(15000).unwrap_or(0), Some(position as u64 + 15000)) // TODO dont hard code things
            .expect("could not fetch in vcf");
        let (molecules, first_var_index, last_var_index) = get_read_molecules(vcf_reader, &vcf_info, READ_TYPE::HIFI);
        for pairing in pairings.iter() {
            swap(cluster_centers, breakpoint, &pairing, 50);
            
            let (_break, _posteriors, log_likelihood) = expectation(&molecules, &cluster_centers);
            log_likelihoods.push(log_likelihood + log_prior);
            swap(cluster_centers, breakpoint, &pairing, 50); // reversing the swap
        }
        let log_bayes_denom = log_sum_exp(&log_likelihoods);
        let log_posterior = log_likelihoods[0] - log_bayes_denom;
        let posterior = log_posterior.exp();
        if posterior < data.long_switch_threshold {
            // end phase block and add to to_return
            to_return.push(PhaseBlock {
                start_index: phase_block_start,
                start_position: vcf_info.variant_positions[phase_block_start],
                end_index: breakpoint,
                end_position: vcf_info.variant_positions[breakpoint],
            });
            phase_block_start = breakpoint + 1;
            let start_position = vcf_info.variant_positions[start_index];
            let end_position = vcf_info.variant_positions[end_index];
            let position = vcf_info.variant_positions[breakpoint];
            vcf_reader
                .fetch(chrom, (position as u64).checked_sub(15000).unwrap_or(0), Some(position as u64))
                .expect("could not fetch in vcf");
                let (molecules, first_var_index, last_var_index)  = get_read_molecules(vcf_reader, &vcf_info, READ_TYPE::HIFI);
            eprintln!("HIT POTENTIAL LONG SWITCH ERROR. phase block from indexes {}-{}, positions {}-{}, posterior {}, breakpoint {} position {} with {} molecules", 
                start_index, end_index, start_position, end_position, posterior, breakpoint, position, molecules.len());
            let (_break, posteriors, log_likelihood) = expectation(&molecules, &cluster_centers);
            //eprintln!("mol posteriors {:?}", posteriors);

            eprintln!("hap1 {:?}", &cluster_centers[0][breakpoint..(breakpoint+10)]);
            eprintln!("hap2 {:?}", &cluster_centers[1][breakpoint..(breakpoint+10)]);
        
        }
        /*** else {
            let start_position = vcf_info.variant_positions[start_index];
            let end_position = vcf_info.variant_positions[end_index];
            let position = vcf_info.variant_positions[breakpoint];
            vcf_reader
                .fetch(chrom, (position as u64).checked_sub(15000).unwrap_or(0), Some(position as u64))
                .expect("could not fetch in vcf");
                let (molecules, first_var_index, last_var_index)  = get_read_molecules(vcf_reader, &vcf_info, READ_TYPE::HIFI);
            let (_break, posteriors, log_likelihood) = expectation(&molecules, &cluster_centers);
            eprintln!("yay we did it right??? posterior {}, molecules {} log likelihoods {:?}", posterior, molecules.len(), log_likelihoods);
        }  ***/
    }
    if to_return.len() == 0 {
        if vcf_info.variant_positions.len() > 0 {
            to_return.push(PhaseBlock{
                start_index: start_index,
                start_position: vcf_info.variant_positions[start_index],
                end_index: end_index,
                end_position: vcf_info.variant_positions[end_index]});
        }
    } else if to_return[to_return.len()-1].end_index != end_index {
        to_return.push(PhaseBlock {
            start_index: phase_block_start,
            start_position: vcf_info.variant_positions[phase_block_start],
            end_index: end_index,
            end_position: vcf_info.variant_positions[end_index],
        });
    }
    to_return
}

fn swap(cluster_centers: &mut Vec<Vec<f32>>, breakpoint: usize, pairing: &Vec<(usize, usize)>, length: usize) {
    let mut touched = [false;32];
    for (hap1, hap2) in pairing {
        if touched[*hap1]  { continue; }
        touched[*hap1] = true;
        touched[*hap2] = true;
        for locus in (breakpoint+1)..(breakpoint+1+length) {
            if locus < cluster_centers[0].len() {
                let tmp = cluster_centers[*hap1][locus];
                cluster_centers[*hap1][locus] = cluster_centers[*hap2][locus];
                cluster_centers[*hap2][locus] = tmp;
            }
        }
    }
}


fn phase_phaseblocks(data: &ThreadData, cluster_centers: &mut Vec<Vec<f32>>, phase_blocks: &Vec<PhaseBlock>) {
    let mut vcf_reader = bcf::IndexedReader::from_path(format!("{}", data.vcf_out.to_string()))
        .expect("could not open indexed vcf reader on output vcf");
    let chrom = vcf_reader
        .header()
        .name2rid(data.chrom.as_bytes())
        .expect("cant get chrom rid");
    let vcf_info = inspect_vcf(&mut vcf_reader, &data);
    let mut phase_block_ids: HashMap<usize,usize> = HashMap::new();
    for (id, phase_block) in phase_blocks.iter().enumerate() {
        //eprintln!("phase block {} from {}-{}",id, phase_block.start_index, phase_block.end_index);
        for i in phase_block.start_index..(phase_block.end_index+1) {
            //eprintln!("\tinserting phase block id {} for vardex {}", id, i);
            phase_block_ids.insert(i,id);
        }
    }
    vcf_reader
        .fetch(chrom, 0, None)
        .expect("some actual error");
    let (hic_reads, _, _) = get_read_molecules(&mut vcf_reader, &vcf_info, READ_TYPE::HIC);
    println!("{} hic reads hitting > 1 variant", hic_reads.len());
    let mut all_counts: HashMap<(usize, usize), HashMap<(u8,u8), usize>> = HashMap::new();
    let mut allele_pair_counts: HashMap<(usize, usize), [u64; 4]> = HashMap::new();
    // ok that is a map from (phase_block_id, phase_block_id) to a map from (pb1_hap, pb2_hap) to counts
    for hic_read in hic_reads {
        for i in 0..hic_read.len() {
            for j in (i+1)..hic_read.len() {
                let allele1 = hic_read[i];
                let allele2 = hic_read[j];
                if !phase_block_ids.contains_key(&allele1.index) || !phase_block_ids.contains_key(&allele2.index) { continue; }
                let phase_block1 = phase_block_ids.get(&allele1.index).expect(&format!("if you are reading this, i screwed up, allele index {} not in a phase block", allele1.index));
                let phase_block2 = phase_block_ids.get(&allele2.index).expect("why didnt the previous one fail first?");
                if phase_block1 != phase_block2 {

                    if allele1.index < allele2.index {
                        let counts = allele_pair_counts.entry((allele1.index, allele2.index)).or_insert([0;4]);
                        if allele1.allele && allele2.allele { // alt and alt
                            counts[0] += 1;
                        } else if allele1.allele && !allele2.allele { // alt and ref
                            counts[1] += 1;
                        } else if !allele1.allele && allele2.allele { // ref and alt
                            counts[2] += 1;
                        } else { // ref and ref
                            counts[3] += 1;
                        }
                    } else {
                        let counts = allele_pair_counts.entry((allele2.index, allele1.index)).or_insert([0;4]);
                        if allele2.allele && allele1.allele { // alt and alt
                            counts[0] += 1;
                        } else if allele2.allele && !allele1.allele { // alt and ref
                            counts[1] += 1;
                        } else if !allele2.allele && allele1.allele { // ref and alt
                            counts[2] += 1;
                        } else { // ref and ref
                            counts[3] += 1;
                        }
                    }
                }
                let mut allele1_haps: Vec<u8> = Vec::new();
                let mut allele2_haps: Vec<u8> = Vec::new();
                for haplotype in 0..cluster_centers.len() {
                    if cluster_centers[haplotype][allele1.index] > 0.95  && allele1.allele { //TODO DONT HARD CODE
                        allele1_haps.push(haplotype as u8);
                    } else if cluster_centers[haplotype][allele1.index] < 0.05  && !allele1.allele {
                        allele1_haps.push(haplotype as u8);
                    }
                    if cluster_centers[haplotype][allele2.index] > 0.95  && allele2.allele { //TODO DONT HARD CODE
                        allele2_haps.push(haplotype as u8);
                    } else if cluster_centers[haplotype][allele2.index] < 0.05  && !allele2.allele {
                        allele2_haps.push(haplotype as u8);
                    }
                }
                
                if phase_block1 != phase_block2 {
                    let min = phase_block1.min(phase_block2);
                    let max = phase_block1.max(phase_block2);
                    let hap_counts = all_counts.entry((*min,*max)).or_insert(HashMap::new());
                    for pb1_hap in &allele1_haps {
                        for pb2_hap in &allele2_haps {
                            let count = hap_counts.entry((*pb1_hap, *pb2_hap)).or_insert(0);
                            *count += 1;
                        }
                    }
                }
            }
        }
    }

    let mut phase_block_pair_phasing_log_likelihoods: HashMap<(usize, usize), HashMap<usize, f64>> = HashMap::new();
    // okay now i have allele_pair_counts which will contribute log likelihoods to phaseblock pairs
    let all_possible_pairings = pairings(data.ploidy); // get all pairings
    let log_phasing_prior = (1.0/(all_possible_pairings.len() as f64)).ln();
    let error = 0.2; // TODO do not hard code
    // each pairing implies a multinomial distribution on each pair of alleles
    for ((allele1_index, allele2_index), counts) in allele_pair_counts.iter() {
        let mut total_counts: u64 = 0;
        for count in counts.iter() {
            total_counts += *count as u64;
        }
        let phase_block1 = phase_block_ids.get(&allele1_index).expect("if you are reading this, i screwed up");
        let phase_block2 = phase_block_ids.get(&allele2_index).expect("why didnt the previous one fail first?");
        //eprintln!("allele pair {} {} hitting phase blocks {} {} with counts {:?}", allele1_index, allele2_index, phase_block1, phase_block2, counts);
        let min = phase_block1.min(phase_block2);
        let max = phase_block1.max(phase_block2);
        let phase_block_log_likelihoods = phase_block_pair_phasing_log_likelihoods.entry((*min, *max)).or_insert(HashMap::new());
        for (pairing_index, haplotype_pairs) in all_possible_pairings.iter().enumerate() {
            let log_likelihood = phase_block_log_likelihoods.entry(pairing_index).or_insert(log_phasing_prior);
            //eprintln!("\tmarriage {}:{:?}",pairing_index, haplotype_pairs);
            let mut pair_probabilities: [f64;4] = [error;4];
            let mut total = 0.0; // for normalization to sum to 1
            //for hap in 0..data.ploidy {
            //    eprintln!("\t\thaplotype {} allele1 frac {}, allele2 frac {}", 
            //        hap, cluster_centers[hap][*allele1_index], cluster_centers[hap][*allele2_index]);
            //}
            for (phase_block1_hap, phase_block2_hap) in haplotype_pairs.iter() {
                let phase_block1_allele_frac = cluster_centers[*phase_block1_hap][*allele1_index] as f64;
                let phase_block2_allele_frac = cluster_centers[*phase_block2_hap][*allele2_index] as f64;
                pair_probabilities[0] += phase_block1_allele_frac * phase_block2_allele_frac;
                pair_probabilities[1] += phase_block1_allele_frac * (1.0 - phase_block2_allele_frac);
                pair_probabilities[2] += (1.0 - phase_block1_allele_frac) * phase_block2_allele_frac;
                pair_probabilities[3] += (1.0 - phase_block1_allele_frac) * (1.0 - phase_block2_allele_frac);
            }
            for psuedocount in pair_probabilities.iter() { total += psuedocount; }
            for index in 0..4 { pair_probabilities[index] /= total; } // normalize to sum to 1.0
            //eprintln!("\t\tfinal probabilities {:?}", pair_probabilities);
            let multinomial_distribution = Multinomial::new(&pair_probabilities, total_counts).unwrap();
           
            *log_likelihood += multinomial_distribution.ln_pmf(counts);
            //eprintln!("\t\t\tlikelihood update {}, in log {}, total log {}",multinomial_distribution.pmf(counts), multinomial_distribution.ln_pmf(counts), log_likelihood);
        }
    }
    
    
    let mut union_find: UnionFind<usize> = UnionFind::new(phase_blocks.len());
    // now for each phase block pair, we will normalize the pairing marginal log likelihoods to sum to 1 giving us
    // posterior probabilities for each marriage
    for ((phase_block1, phase_block2), marriage_log_likelihoods) in phase_block_pair_phasing_log_likelihoods.iter() {
        let mut log_likelihoods:Vec<f32> = Vec::new();
        let mut posteriors: Vec<f32> = Vec::new();
        for i in 0..marriage_log_likelihoods.len() {
            log_likelihoods.push(0.0);
            posteriors.push(0.0);
        }
        for (pairing_index, log_likelihood) in marriage_log_likelihoods.iter() {
            log_likelihoods[*pairing_index] = *log_likelihood as f32;
        }
        let log_denominator = log_sum_exp(&log_likelihoods);
        let mut max = 0.0;
        let mut max_index = 0;
        for i in 0..marriage_log_likelihoods.len() {
            posteriors[i] = (log_likelihoods[i] - log_denominator).exp();
            if posteriors[i] >= max {
                max = posteriors[i];
                max_index = i;
            }
        }
        eprintln!("after normalizing, phase blocks {} and {} with pairing {} and posterior {}", phase_block1, phase_block2, max_index, max);
        if max > data.hic_phasing_posterior_threshold {
            union_find.union(*phase_block1, *phase_block2);
        }
    }

    let mut problem_phaseblocks: HashMap<usize, usize> = HashMap::new();// map from phase block to the number of triangles it ruins
    for ((phase_block1, phase_block2), marriage_log_likelihoods) in phase_block_pair_phasing_log_likelihoods.iter() {
        for ((phase_block3, phase_block4), marriage_log_likelihoods2) in phase_block_pair_phasing_log_likelihoods.iter() {
            
            if phase_block1 == phase_block3 {
                if union_find.equiv(*phase_block1, *phase_block2) && 
                    union_find.equiv(*phase_block2, *phase_block3) && 
                    union_find.equiv(*phase_block3, *phase_block4) {
                    let (edge1, post) = get_posterior(marriage_log_likelihoods);
                    let (edge2, post2) = get_posterior(marriage_log_likelihoods2);
                    let min = phase_block2.min(phase_block4);
                    let max = phase_block2.max(phase_block4);
                    match phase_block_pair_phasing_log_likelihoods.get(&(*min, *max)) {
                        Some(marriage_log_likelihoods3) => {
                            let (edge3, post3) = get_posterior(marriage_log_likelihoods3);
                            let mut problem = false;
                            if edge1 == 0 && edge2 == 0 && edge3 == 1 {
                                eprintln!("yeah we have a problem");
                                problem = true;
                            } else if edge1 == 0 && edge2 == 1 && edge3 == 0 {
                                eprintln!("yeah we have a problem2");
                                problem = true;
                            } else if edge1 == 1 && edge2 == 0 && edge3 == 0 {
                                problem = true;
                                eprintln!("yeah we have a problem3");
                            } else if edge1 == 1 && edge2 == 1 && edge3 == 1 {
                                eprintln!("yeah we have a problem4");
                                problem = true;
                            } else {
                                eprintln!("triangle {} - {} - {} consistent", edge1, edge2, edge3);
                            }
                            if problem {
                                let count = problem_phaseblocks.entry(*phase_block1).or_insert(0);
                                *count += 1;
                                let count = problem_phaseblocks.entry(*phase_block2).or_insert(0);
                                *count += 1;
                                let count = problem_phaseblocks.entry(*phase_block4).or_insert(0);
                                *count += 1;
                            }
                        },
                        None => (),
                    }
                }
            }
        }
    }
    let mut count_vec: Vec<(&usize, &usize)> = problem_phaseblocks.iter().collect();
    count_vec.sort_by(|a, b| b.1.cmp(a.1));


    println!("problem phaseblocks ruining triangles {:?}", count_vec);


    let labeling = union_find.into_labeling();
    let mut new_phaseblocks: HashMap<usize, Vec<usize>> = HashMap::new();
    for (phase_block_id, label) in labeling.iter().enumerate() {
        let new_phaseblock = new_phaseblocks.entry(*label).or_insert(Vec::new());
        new_phaseblock.push(phase_block_id);
    }
    // get phaseblock N50... 
    let mut sizes: Vec<usize> = Vec::new();
    let mut total: usize = 0;
    for (new_id, old_phase_block_ids) in new_phaseblocks.iter() {
        let mut total_length = 0;
        for old_block_id in old_phase_block_ids {
            let length = phase_blocks[*old_block_id].end_position - phase_blocks[*old_block_id].start_position;
            total_length += length;
        }
        sizes.push(total_length);
        total += total_length;
    }
    println!("after hic phasing we have {} phase blocks for chrom {}", new_phaseblocks.len(), data.chrom);
    sizes.sort_by(|a, b| b.cmp(a));
    println!("{:?}", sizes);
    println!("total length of phased region for chrom {} is {} vs chrom length of {}", data.chrom, total, data.chrom_length);
    let mut so_far = 0;
    for size in sizes {
        so_far += size;
        if so_far > total/2 {
            println!("After hic phasing the N50 phase blocks for chrom {} is {}", data.chrom, size);
            break;
        }
    }
    
}

fn get_posterior(marriage_log_likelihoods: &HashMap<usize,f64>) -> (usize, f32) {
    let mut log_likelihoods:Vec<f32> = Vec::new();
    let mut posteriors: Vec<f32> = Vec::new();
    for i in 0..marriage_log_likelihoods.len() {
        log_likelihoods.push(0.0);
        posteriors.push(0.0);
    }
    for (pairing_index, log_likelihood) in marriage_log_likelihoods.iter() {
        log_likelihoods[*pairing_index] = *log_likelihood as f32;
    }
    let log_denominator = log_sum_exp(&log_likelihoods);
    let mut max = 0.0;
    let mut max_index = 0;
    for i in 0..marriage_log_likelihoods.len() {
        posteriors[i] = (log_likelihoods[i] - log_denominator).exp();
        if posteriors[i] >= max {
            max = posteriors[i];
            max_index = i;
        }
    }
    (max_index, max)
}


fn do_something(counts: &HashMap<(u8,u8),usize>, ploidy: usize) {
    let all_possible_pairings = pairings(ploidy); // get all pairings
    // each pairing implies a multinomial distribution on each pair of alleles
    
    if ploidy == 2 { // diploid special case TODO consider making diploid and polyploid generalized?
        // but maybe the diploid case naturally has more power due to the assumed exclusivity of on allele
        // on each haplotype
        let cis1 = counts.get(&(0,0)).unwrap_or(&0);
        let cis2 = counts.get(&(1,1)).unwrap_or(&0);
        let trans1 = counts.get(&(0,1)).unwrap_or(&0);
        let trans2 = counts.get(&(1,0)).unwrap_or(&0);
        let cis = cis1 + cis2;
        let trans = trans1 + trans2;
        let p_value = binomial_test(cis, trans);
        if p_value < 0.0001 { // TODO DONT HARD CODE
            if cis > trans {
                assert!((*cis1.min(cis2) as f32) / (cis as f32) > 0.15, 
                    "something is weird here, minor allele fraction is small in hic phasing");
            } else {
                assert!((*trans1.min(trans2) as f32) / (trans as f32) > 0.15, 
                    "something is weird here, minor allele fraction is small in hic phasing");
            }
        }
        // do something
    } else { // polyploid case
        // create ordering of preferences of haplotypes in phaseblock2 for haplotypes in phaseblock1
        let (hap1_preferences, hap2_preferences) = 
            get_marriage_preferences(&counts, ploidy);
        let mut hap1_rankings: Vec<HashMap<usize, usize>> = Vec::new();
        let mut hap2_rankings: Vec<HashMap<usize, usize>> = Vec::new();
        for hap1 in 0..ploidy {
            let mut rankings: HashMap<usize, usize> = HashMap::new();
            for (rank, (_, hap2)) in hap1_preferences[hap1].iter().enumerate() {
                rankings.insert(*hap2, rank);
            }
            hap1_rankings.push(rankings);
        }
        for hap2 in 0..ploidy {
            let mut rankings: HashMap<usize, usize> = HashMap::new();
            for (rank, (_, hap1)) in hap2_preferences[hap2].iter().enumerate() {
                rankings.insert(*hap1, rank);
            }
            hap2_rankings.push(rankings);
        }
        let mut has_proposed_to: Vec<HashSet<usize>> = Vec::new();
        for _h1 in 0..ploidy { has_proposed_to.push(HashSet::new()); }
        let mut suitor_engagements: HashMap<usize, usize> = HashMap::new();
        let mut maiden_engagements: HashMap<usize, usize> = HashMap::new(); 

        // step 1: In the first round, first a) each unengaged man proposes to the woman he prefers most, 
        // and then b) each woman replies "maybe" to her suitor she most prefers and "no" to all other 
        // suitors. She is then provisionally "engaged" to the suitor she most prefers so far, 
        // and that suitor is likewise provisionally engaged to her.
        let mut maidens_proposals: Vec<HashSet<usize>> = Vec::new(); 
        // each haplotype2_maiden could have multiple proposals
        for _hap2_maiden in 0..ploidy { maidens_proposals.push(HashSet::new()); }
        for haplotype1_suitor in 0..ploidy {
            for (_, haplotype2_maiden) in hap1_preferences[haplotype1_suitor].iter() {
                has_proposed_to[haplotype1_suitor].insert(*haplotype2_maiden);
                maidens_proposals[*haplotype2_maiden].insert(haplotype1_suitor);
            }
        }
        for haplotype2_maiden in 0..ploidy {
            for (_, haplotype1_suitor) in hap2_preferences[haplotype2_maiden].iter() {
                if maidens_proposals[haplotype2_maiden].contains(haplotype1_suitor) {
                    suitor_engagements.insert(*haplotype1_suitor, haplotype2_maiden);
                    maiden_engagements.insert(haplotype2_maiden, *haplotype1_suitor);
                    break;
                }
            }
        }
        // steps 2 and on
        // In each subsequent round, first a) each unengaged man proposes to the most-preferred woman 
        // to whom he has not yet proposed (regardless of whether the woman is already engaged), 
        // and then b) each woman replies "maybe" if she is currently not engaged or if she prefers 
        // this man over her current provisional partner (in this case, she rejects her current 
        // provisional partner who becomes unengaged). The provisional nature of engagements preserves 
        // the right of an already-engaged woman to "trade up" (and, in the process, to "jilt" 
        // her until-then partner).
        while maiden_engagements.len() < ploidy {
            let mut maidens_proposals: Vec<HashSet<usize>> = Vec::new(); 
            // each haplotype2_maiden could have multiple proposals
            for hap2_maiden in 0..ploidy { maidens_proposals.push(HashSet::new()); }

            for haplotype1_suitor in 0..ploidy {
                if suitor_engagements.contains_key(&haplotype1_suitor) { continue; }
                for (_, haplotype2_maiden) in hap1_preferences[haplotype1_suitor].iter() {
                    if has_proposed_to[haplotype1_suitor].contains(haplotype2_maiden) { 
                        continue; 
                    } else {
                        has_proposed_to[haplotype1_suitor].insert(*haplotype2_maiden);
                        maidens_proposals[*haplotype2_maiden].insert(haplotype1_suitor);
                    }
                }
            }

            for haplotype2_maiden in 0..ploidy {
                let mut best_proposal: Option<usize> = None;
                for (_, haplotype1_suitor) in hap2_preferences[haplotype2_maiden].iter() {
                    if maidens_proposals[haplotype2_maiden].contains(haplotype1_suitor) {
                        best_proposal = Some(*haplotype1_suitor);
                        break;
                    }
                }
                match best_proposal {
                    Some(hap1_suitor) => {
                        if !maiden_engagements.contains_key(&haplotype2_maiden) {
                            maiden_engagements.insert(haplotype2_maiden, hap1_suitor);
                        } else {
                            let new_suitor_ranking = hap2_rankings[haplotype2_maiden].get(&hap1_suitor).unwrap_or(&usize::MAX);
                            let former_suitor = *maiden_engagements.get(&haplotype2_maiden).unwrap();
                            let former_suitor_ranking = hap2_rankings[haplotype2_maiden].get(&former_suitor).unwrap_or(&usize::MAX);
                            if new_suitor_ranking < former_suitor_ranking {
                                // jilting former suitor
                                maiden_engagements.insert(haplotype2_maiden, hap1_suitor);
                                suitor_engagements.insert(hap1_suitor, haplotype2_maiden);
                                suitor_engagements.remove(&former_suitor);
                            }
                        }
                    },
                    None => (),
                }
            }
        }
    }
}



fn get_marriage_preferences(counts: &HashMap<(u8,u8),usize>, ploidy: usize) -> 
    (Vec<Vec<(usize, usize)>>, Vec<Vec<(usize, usize)>>) { 
    let mut hap1_preferences: Vec<Vec<(usize, usize)>> = Vec::new();// (counts, haplotype2)
    let mut hap2_preferences: Vec<Vec<(usize, usize)>> = Vec::new();
    for haplotype1 in 0..ploidy {
        let mut preference_counts: Vec<(usize, usize)> = Vec::new(); // (counts, haplotype2)
        for haplotype2 in 0..ploidy {
            let count = counts.get(&(haplotype1 as u8, haplotype2 as u8)).unwrap_or(&0);
            preference_counts.push((*count, haplotype2));
        }
        preference_counts.sort();
        preference_counts.reverse();
        hap1_preferences.push(preference_counts);
    }
    for haplotype2 in 0..ploidy {
        let mut preference_counts: Vec<(usize, usize)> = Vec::new(); // (counts, haplotype2)
        for haplotype1 in 0..ploidy {
            let count = counts.get(&(haplotype1 as u8, haplotype2 as u8)).unwrap_or(&0);
            preference_counts.push((*count, haplotype2));
        }
        preference_counts.sort();
        preference_counts.reverse();
        hap2_preferences.push(preference_counts);
    }
    (hap1_preferences, hap2_preferences)
}


fn pairings(size: usize) -> Vec<Vec<(usize, usize)>> {
    let pairings_so_far: Vec<(usize, usize)> = Vec::new();
    let left: Vec<usize> = (0..size).collect();
    let right: Vec<usize> = (0..size).collect();
    return generate_pairings(pairings_so_far, left, right);
}


// generates all bipartite graph pairings
fn generate_pairings(pairings_so_far: Vec<(usize, usize)>, left_remaining: Vec<usize>, right_remaining: Vec<usize>) -> Vec<Vec<(usize, usize)>> {
    assert!(left_remaining.len() == right_remaining.len());
    let mut to_return: Vec<Vec<(usize, usize)>> = Vec::new();
    if left_remaining.len() == 0 {
        to_return.push(pairings_so_far);
        return to_return;
    }
    let left = &left_remaining[0];
    for right in right_remaining.iter() {
        let mut so_far = pairings_so_far.to_vec();
        so_far.push((*left, *right));
        let mut new_left: Vec<usize> = Vec::new();
        for l in left_remaining.iter() {
            if l != left {
                new_left.push(*l);
            }
        }
        let mut new_right: Vec<usize> = Vec::new();
        for r in right_remaining.iter() {
            if r != right {
                new_right.push(*r);
            }
        }
        let new_pairings = generate_pairings(so_far, new_left, new_right);
        for pairing in new_pairings {
            to_return.push(pairing);
        }
    }
    return to_return;
}

fn binomial_test(cis: usize, trans: usize) -> f64 {
    let min = cis.min(trans) as u64;
    let n = Binomial::new(0.5, (cis+trans) as u64).unwrap();
    let p_value = n.cdf(min) * 2.0;
    p_value
}

fn output_phased_vcf(
    data: &ThreadData,
    cluster_centers: Vec<Vec<f32>>,
    phase_blocks: Vec<PhaseBlock>,
) {
    let mut vcf_reader = bcf::IndexedReader::from_path(format!("{}", data.vcf_out.to_string()))
        .expect("could not open indexed vcf reader on output vcf");
    let header_view = vcf_reader.header();
    let mut new_header = bcf::header::Header::from_template(header_view);
    new_header.push_record(br#"##FORMAT=<ID=PS,Number=1,Type=Integer,Description="phase set id">"#);
    let mut vcf_writer = bcf::Writer::from_path(
        format!("{}/phased_chrom_{}.vcf.gz", data.output, sanitize_filename::sanitize(&data.chrom)),
        &new_header,
        false,
        Format::Vcf,
    )
    .expect("could not open vcf writer");
    let mut index: usize = 0;
    let mut index_to_phase_block: HashMap<usize, usize> = HashMap::new();
    for (id, phase_block) in phase_blocks.iter().enumerate() {
        for i in phase_block.start_index..(phase_block.end_index + 1) {
            index_to_phase_block.insert(i, id);
        }
    }
    for r in vcf_reader.records() {
        let mut rec = r.expect("could not unwrap vcf record");
        let mut new_rec = vcf_writer.empty_record();
        copy_vcf_record(&mut new_rec, &rec);
        //println!("{}",index);
        match index_to_phase_block.get(&index) {
            Some(id) => new_rec.push_format_integer(b"PS", &[*id as i32]).expect("you did it again, pushing your problems down to future you"),
            None => (),
        }
        //let phase_block_id = index_to_phase_block.get(&index).expect("i had it coming");
        let genotypes = infer_genotype(&cluster_centers, index);
        new_rec.push_genotypes(&genotypes)
            .expect("i did expect this error");
        vcf_writer.write(&new_rec).expect("could not write record");
        index += 1;
    }
}

fn infer_genotype(cluster_centers: &Vec<Vec<f32>>, index: usize) -> Vec<GenotypeAllele> {
    let mut genotypes: Vec<GenotypeAllele> = Vec::new();
    for haplotype in 0..cluster_centers.len() {
        if cluster_centers[haplotype][index] > 0.95 {
            genotypes.push(GenotypeAllele::Phased(1));
        } else if cluster_centers[haplotype][index] < 0.05 {
            genotypes.push(GenotypeAllele::Phased(0));
        } else if cluster_centers[haplotype][index] <= 0.5 {
            genotypes.push(GenotypeAllele::Unphased(0));
        } else {
            genotypes.push(GenotypeAllele::Unphased(1));
        }
    }
    genotypes
}

fn maximization(
    molecules: &Vec<Vec<Allele>>,
    posteriors: Vec<Vec<f32>>,
    cluster_centers: &mut Vec<Vec<f32>>,
    min_index: &mut usize,
    max_index: &mut usize,
) -> f32 {
    let mut updates: HashMap<usize, Vec<(f32, f32)>> = HashMap::new(); // variant index to vec across
    let mut variant_molecule_count: HashMap<usize, usize> = HashMap::new();
    *min_index = std::usize::MAX;
    *max_index = 0;
    // haplotype clusters to a tuple of (numerator, denominator)
    for molecule_index in 0..molecules.len() {
        // if molecule does not support any haplotype over another, dont use it in maximization
        let mut different = false;
        for haplotype in 0..cluster_centers.len() {
            if posteriors[molecule_index][haplotype] != posteriors[molecule_index][0] {
                different = true;
            }
        }
        if !different {
            //println!("debug, molecule does not support any haplotype over another");
            continue;
        }
        for allele in &molecules[molecule_index] {
            let variant_index = allele.index;
            let alt = allele.allele;
            let count = variant_molecule_count.entry(variant_index).or_insert(0);
            *count += 1;
            for haplotype in 0..cluster_centers.len() {
                let posterior = posteriors[molecule_index][haplotype];
                let numerators_denominators = updates
                    .entry(variant_index)
                    .or_insert(vec![(0.0, 0.0); cluster_centers.len()]);
                if alt {
                    numerators_denominators[haplotype].0 += posterior;
                    numerators_denominators[haplotype].1 += posterior;
                } else {
                    numerators_denominators[haplotype].1 += posterior;
                }
            }
        }
    }
    let mut total_change = 0.0;
    for (variant_index, haplotypes) in updates.iter() {
        if variant_molecule_count.get(variant_index).unwrap() > &3 { // TODO dont hard code, make parameters
            //TODO Dont hard code stuff
            *min_index = (*min_index).min(*variant_index);
            *max_index = (*max_index).max(*variant_index);
            for haplotype in 0..haplotypes.len() {
                let numerators_denominators = haplotypes[haplotype];
                let allele_fraction = (numerators_denominators.0 / numerators_denominators.1)
                    .max(0.001)
                    .min(0.999);
                total_change +=
                    (allele_fraction - cluster_centers[haplotype][*variant_index]).abs();
                cluster_centers[haplotype][*variant_index] = allele_fraction;
            }
        }
    }
    total_change
}

// molecules is vec of molecules each a vec of alleles
// cluster centers is vec of haplotype clusters  by variant loci
// posteriors (return value) is molecules by clusters to posterior probability
fn expectation(
    molecules: &Vec<Vec<Allele>>,
    cluster_centers: &Vec<Vec<f32>>,
) -> (bool, Vec<Vec<f32>>, f32) {
    let mut posteriors: Vec<Vec<f32>> = Vec::new();
    let mut any_different = false;
    let mut log_likelihood: f32 = 0.0;
    for molecule in molecules.iter() {
        let mut log_probs: Vec<f32> = Vec::new(); // for each haplotype
        for haplotype in 0..cluster_centers.len() {
            let mut log_prob = 0.0; // log(0) = probability 1
            for allele in molecule.iter() {
                if allele.allele {
                    // alt allele
                    log_prob += cluster_centers[haplotype][allele.index].ln(); // adding in log space, multiplying in probability space
                } else {
                    log_prob += (1.0 - cluster_centers[haplotype][allele.index]).ln();
                }
            }
            
            log_probs.push(log_prob);
        }
        let bayes_log_denom = log_sum_exp(&log_probs);
        log_likelihood += bayes_log_denom;
        let mut mol_posteriors: Vec<f32> = Vec::new();

        for log_prob in log_probs {
            mol_posteriors.push((log_prob - bayes_log_denom).exp());
        }

        for haplotype in 0..cluster_centers.len() {
            if mol_posteriors[haplotype] != mol_posteriors[0] {
                any_different = true;
            }
        }

        posteriors.push(mol_posteriors);
    }
    (!any_different, posteriors, log_likelihood)
}

fn log_sum_exp(p: &Vec<f32>) -> f32 {
    let max_p: f32 = p.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
    let sum_rst: f32 = p.iter().map(|x| (x - max_p).exp()).sum();
    max_p + sum_rst.ln()
}

enum READ_TYPE {
    HIFI, HIC,
}

fn get_read_molecules(vcf: &mut bcf::IndexedReader, vcf_info: &VCF_info, read_type: READ_TYPE) -> (Vec<Vec<Allele>>, usize, usize) {
    let mut molecules: HashMap<String, Vec<Allele>> = HashMap::new();
    let ref_tag;
    let alt_tag;
    match read_type {
        READ_TYPE::HIFI => {
            ref_tag = b"RM"; 
            alt_tag = b"AM";
        },
        READ_TYPE::HIC => {
            ref_tag = b"RH";
            alt_tag = b"AH";
        }
    }
    let mut last_var_index = 0;
    let mut first_var_index = usize::MAX;
    for rec in vcf.records() {
        let rec = rec.expect("couldnt unwrap record");
        let pos = rec.pos();
        let var_index = vcf_info
            .position_to_index
            .get(&(pos as usize))
            .expect("please don't do this to me");
        last_var_index = *var_index;
        if first_var_index == usize::MAX { first_var_index = *var_index; }

        match rec.format(ref_tag).string() { 
            Ok(rec_format) => {
                for rec in rec_format.iter() {
                    let ref_mols = std::str::from_utf8(rec).expect(":").to_string();
                    for read in ref_mols.split(";") {
                        let mol = molecules.entry(read.to_string()).or_insert(Vec::new());
                        mol.push(Allele {
                            index: *var_index,
                            allele: false,
                        });
                    }
                }
            }
            Err(_) => (),
        }
        match rec.format(alt_tag).string() {
            Ok(rec_format) => {
                for rec in rec_format.iter() {
                    let alt_mols = std::str::from_utf8(rec).expect(":").to_string();
                    for read in alt_mols.split(";") {
                        let mol = molecules.entry(read.to_string()).or_insert(Vec::new());
                        mol.push(Allele {
                            index: *var_index,
                            allele: true,
                        });
                    }
                }
            }
            Err(_) => (),
        }
    }
    let mut to_return: Vec<Vec<Allele>> = Vec::new();
    for (_read_name, alleles) in molecules.iter() {
        if alleles.len() < 2 {
            continue;
        }
        let mut mol: Vec<Allele> = Vec::new();
        for allele in alleles {
            mol.push(*allele);
        }
        to_return.push(mol);
    }
    (to_return, first_var_index, last_var_index)
}

#[derive(Clone, Copy)]
struct Allele {
    index: usize,
    allele: bool, // alt is true, ref is false
}

struct VCF_info {
    num_variants: usize,
    final_position: i64,
    variant_positions: Vec<usize>,
    position_to_index: HashMap<usize, usize>,
}

fn inspect_vcf(vcf: &mut bcf::IndexedReader, data: &ThreadData) -> VCF_info {
    let chrom = vcf
        .header()
        .name2rid(data.chrom.as_bytes())
        .expect("cant get chrom rid");
    vcf.fetch(chrom, 0, None).expect("could not fetch in vcf");
    let mut position_to_index: HashMap<usize, usize> = HashMap::new();
    let mut num = 0;
    let mut last_pos = 0;
    let mut variant_positions: Vec<usize> = Vec::new();
    for r in vcf.records() {
        let rec = r.expect("could not unwrap vcf record");
        last_pos = rec.pos();
        variant_positions.push(rec.pos() as usize);
        position_to_index.insert(rec.pos() as usize, num);
        num += 1;
    }
    println!("vcf has {} variants for chrom {}", num, chrom);
    VCF_info {
        num_variants: num,
        final_position: last_pos,
        variant_positions: variant_positions,
        position_to_index: position_to_index,
    }
}

fn init_cluster_centers(num: usize, data: &ThreadData) -> Vec<Vec<f32>> {
    let seed = [data.seed; 32];
    let mut rng: StdRng = SeedableRng::from_seed(seed);
    let mut cluster_centers: Vec<Vec<f32>> = Vec::new();
    for k in 0..data.ploidy {
        let mut cluster_center: Vec<f32> = Vec::new();
        for v in 0..num {
            cluster_center.push(0.5);
        }
        if cluster_center.len() > 0 {
            cluster_center[0] = rng.gen::<f32>().min(0.98).max(0.02);
            cluster_centers.push(cluster_center);
        }
    }
    cluster_centers
}

fn get_all_variant_assignments(data: &ThreadData) -> Result<(), Error> {
    let mut long_read_bam_reader = match &data.long_read_bam {
        Some(x) => Some(bam::IndexedReader::from_path(x).expect("could not open bam, maybe no index?")),
        None => None,
    };
    let mut hic_bam_reader = match &data.hic_bam {
        Some(x) => Some(bam::IndexedReader::from_path(x).expect("could not open bam, maybe no index2?")),
        None => None,
    };
    let mut fasta = fasta::IndexedReader::from_file(&data.fasta).expect("cannot open fasta file");

    let mut vcf_reader = bcf::IndexedReader::from_path(data.vcf.to_string()).expect("could not load index for vcf... looking for .csi file");
    let header_view = vcf_reader.header();
    let mut new_header = bcf::header::Header::from_template(header_view);
    new_header.push_record(br#"##fileformat=VCFv4.2"#);
    new_header.push_record(br#"##FORMAT=<ID=AM,Number=1,Type=String,Description="alt molecules long reads">"#);
    new_header.push_record(br#"##FORMAT=<ID=RM,Number=1,Type=String,Description="ref molecules long reads">"#);
    new_header.push_record(br#"##FORMAT=<ID=AH,Number=1,Type=String,Description="alt molecules hic">"#);
    new_header.push_record(br#"##FORMAT=<ID=RH,Number=1,Type=String,Description="ref molecules hic">"#);

    {
        // creating my own scope to close later to close vcf writer
        let mut vcf_writer =
            bcf::Writer::from_path(data.vcf_out.to_string(), &new_header, false, Format::Vcf).expect("cant open vcf writer");
        let chrom = vcf_reader.header().name2rid(data.chrom.as_bytes()).expect("could not read vcf header");
        match vcf_reader.fetch(chrom, 0, None) {
            Ok(_) => {
                let mut total = 0;
                let mut hets = 0;
                for (i, _rec) in vcf_reader.records().enumerate() {
                    total += 1;
                    let rec = _rec.expect("cant unwrap vcf record");
                    let pos = rec.pos();
                    let alleles = rec.alleles();
                    let mut new_rec = vcf_writer.empty_record();
                    copy_vcf_record(&mut new_rec, &rec);
                    if alleles.len() > 2 {
                        continue; // ignore multi allelic sites
                    }
                    let reference = std::str::from_utf8(alleles[0]).expect("this really shouldnt fail");
                    let alternative = std::str::from_utf8(alleles[1]). expect("this really shouldnt fail2");
                    let rec_chrom = rec.rid().expect("could not unwrap vcf record id");

                    let genotypes = rec.genotypes().expect("cant get genotypes");
                    let genotype = genotypes.get(0); // assume only 1 and get the first one
                    if is_heterozygous(genotype) {
                        hets += 1;
                        get_variant_assignments(
                            &data.chrom,
                            pos as usize,
                            reference.to_string(),
                            alternative.to_string(),
                            data.min_mapq,
                            data.min_base_qual,
                            &mut long_read_bam_reader,
                            &mut hic_bam_reader,
                            &mut fasta,
                            data.window,
                            data.chrom_length,
                            &mut vcf_writer,
                            &mut new_rec,
                        );
                    }
                    //if hets > 2000 {
                    //    break;
                    //} //TODO remove, for small example
                }
                println!(
                    "done, saw {} records of which {} were hets in chrom {}",
                    total, hets, data.chrom
                );
            },
            Err(_e) => (),
        }; // skip to chromosome for this thread
        
    } //creating my own scope to close vcf

    let result = Command::new("bcftools")
        .args(&["index", "-f", &data.vcf_out])
        .status()
        .expect("bcftools failed us");

    

    fs::File::create(data.vcf_out_done.to_string()).expect("cant create .done file. are the permissions wrong?");
    Ok(())
}

fn copy_vcf_record(new_rec: &mut bcf::record::Record, rec: &bcf::record::Record) {
    new_rec.set_rid(rec.rid());
    new_rec.set_pos(rec.pos());
    new_rec.set_id(&rec.id());
    for filter in rec.filters() {
        new_rec.push_filter(&filter);
    }
    new_rec
        .set_alleles(&rec.alleles())
        .expect("could not write alleles to new record???");
    new_rec.set_qual(rec.qual());
    let header = rec.header();
    for header_record in header.header_records() {
        match header_record {
            bcf::header::HeaderRecord::Filter { key, values } => {}
            bcf::header::HeaderRecord::Info { key, values } => {
                let mut format = FORMAT {
                    Id: "blah".to_string(),
                    Type: FORMAT_TYPE::Integer,
                };
                for (x, y) in values {
                    match x.as_str() {
                        "ID" => format.Id = y,
                        "Type" => match y.as_str() {
                            "Integer" => format.Type = FORMAT_TYPE::Integer,
                            "Float" => format.Type = FORMAT_TYPE::Float,
                            "String" => format.Type = FORMAT_TYPE::String,
                            "Char" => format.Type = FORMAT_TYPE::Char,
                            &_ => (),
                        },
                        &_ => (),
                    }
                }
                match format.Type {
                    FORMAT_TYPE::Integer => match rec.info(&format.Id.as_bytes()).integer() {
                        Ok(rec_format) => {
                            for thingy in rec_format.iter() {
                                new_rec
                                    .push_info_integer(&format.Id.as_bytes(), thingy)
                                    .expect("fail1");
                            }
                        }
                        Err(_) => (),
                    },
                    FORMAT_TYPE::Float => match rec.info(&format.Id.as_bytes()).float() {
                        Ok(rec_format) => {
                            for thingy in rec_format.iter() {
                                new_rec
                                    .push_info_float(&format.Id.as_bytes(), thingy)
                                    .expect("fail1");
                            }
                        }
                        Err(_) => (),
                    },
                    FORMAT_TYPE::String => match rec.info(&format.Id.as_bytes()).string() {
                        Ok(rec_format) => {
                            new_rec
                                .push_info_string(
                                    &format.Id.as_bytes(),
                                    &rec_format.expect("blerg"),
                                )
                                .expect("fail1");
                        }
                        Err(_) => (),
                    },
                    FORMAT_TYPE::Char => match rec.info(&format.Id.as_bytes()).string() {
                        Ok(rec_format) => {
                            new_rec
                                .push_info_string(
                                    &format.Id.as_bytes(),
                                    &rec_format.expect("blerg2"),
                                )
                                .expect("fail1");
                        }
                        Err(_) => (),
                    },
                }
            }
            bcf::header::HeaderRecord::Format { key, values } => {
                let mut format = FORMAT {
                    Id: "blah".to_string(),
                    Type: FORMAT_TYPE::Integer,
                };
                for (x, y) in values {
                    match x.as_str() {
                        "ID" => format.Id = y,
                        "Type" => match y.as_str() {
                            "Integer" => format.Type = FORMAT_TYPE::Integer,
                            "Float" => format.Type = FORMAT_TYPE::Float,
                            "String" => format.Type = FORMAT_TYPE::String,
                            "Char" => format.Type = FORMAT_TYPE::Char,
                            &_ => (),
                        },
                        &_ => (),
                    }
                }
                match format.Type {
                    FORMAT_TYPE::Integer => match rec.format(&format.Id.as_bytes()).integer() {
                        Ok(rec_format) => {
                            for thingy in rec_format.iter() {
                                new_rec
                                    .push_format_integer(&format.Id.as_bytes(), thingy)
                                    .expect("noooooooooo");
                            }
                        }
                        Err(_) => (),
                    },
                    FORMAT_TYPE::Float => match rec.format(&format.Id.as_bytes()).float() {
                        Ok(rec_format) => {
                            for thingy in rec_format.iter() {
                                new_rec
                                    .push_format_float(&format.Id.as_bytes(), thingy)
                                    .expect("fail1");
                            }
                        }
                        Err(_) => (),
                    },
                    FORMAT_TYPE::String => {
                        if format.Id == "GT".to_string() {
                            let sample_count = header.sample_count();
                            for i in 0..sample_count {
                                let gt = rec.genotypes().expect("lkjlkj").get(i as usize);
                                new_rec.push_genotypes(&gt);
                            }
                        } else {
                            match rec.format(&format.Id.as_bytes()).string() {
                                Ok(rec_format) => {
                                    new_rec
                                        .push_format_string(&format.Id.as_bytes(), &rec_format)
                                        .expect("fail1");
                                }
                                Err(_) => (),
                            }
                        }
                    }
                    FORMAT_TYPE::Char => match rec.format(&format.Id.as_bytes()).string() {
                        Ok(rec_format) => {
                            new_rec
                                .push_format_string(&format.Id.as_bytes(), &rec_format)
                                .expect("fail1");
                        }
                        Err(_) => (),
                    },
                }
            }
            bcf::header::HeaderRecord::Contig { key, values } => {}
            bcf::header::HeaderRecord::Structured { key, values } => {}
            bcf::header::HeaderRecord::Generic { key, value } => {}
        }
    }
}

struct FORMAT {
    Id: String,
    Type: FORMAT_TYPE,
}

#[derive(Debug)]
enum FORMAT_TYPE {
    Integer,
    Float,
    String,
    Char,
}

fn get_variant_assignments<'a>(
    chrom: &String,
    pos: usize,
    ref_allele: String,
    alt_allele: String,
    min_mapq: u8,
    min_base_qual: u8,
    long_reads: &mut Option<bam::IndexedReader>,
    hic_reads: &mut Option<bam::IndexedReader>,
    fasta: &mut fasta::IndexedReader<std::fs::File>,
    window: usize,
    chrom_length: u64,
    vcf_writer: &mut bcf::Writer,
    vcf_record: &mut bcf::record::Record,
) {
    if (pos + window) as u64 > chrom_length {
        return;
    }
    match long_reads {
        Some(bam) => {
            let (ref_string, alt_string) = get_read_assignments(chrom.to_string(), pos, bam, 
                fasta, &ref_allele, &alt_allele, min_base_qual, min_mapq, window);
            vcf_record
                .push_format_string(b"AM", &[alt_string.as_bytes()])
                .expect("blarg");
            vcf_record
                .push_format_string(b"RM", &[ref_string.as_bytes()])
                .expect("gggg");
        }
        None => (),
    }
    match hic_reads {
        Some(bam) => {
            let (ref_string, alt_string) = get_read_assignments(chrom.to_string(), pos, bam, 
                fasta, &ref_allele, &alt_allele, min_base_qual, min_mapq, window);
            vcf_record
                .push_format_string(b"AH", &[alt_string.as_bytes()])
                .expect("blarg2");
            vcf_record
                .push_format_string(b"RH", &[ref_string.as_bytes()])
                .expect("gggg2");
        }
        None => (),
    }
    vcf_writer.write(vcf_record).expect("nope");
}

fn get_read_assignments(
    chrom: String,
    pos: usize,
    bam: &mut bam::IndexedReader,
    fasta: &mut fasta::IndexedReader<std::fs::File>,
    ref_allele: &String,
    alt_allele: &String,
    min_base_qual: u8,
    min_mapq: u8,
    window: usize,
) -> (String, String) {
    let tid = bam
        .header()
        .tid(chrom.as_bytes())
        .expect(&format!("cannot find chrom tid {}", chrom));
    bam.fetch((tid, pos as u32, (pos + 1) as u32))
        .expect("blah"); // skip to region of bam of this variant position
    let ref_start = (pos - window) as u64;
    let ref_end = (pos + window + ref_allele.len()) as u64;
    fasta
        .fetch(&chrom, ref_start, ref_end)
        .expect("fasta fetch failed");
    let mut ref_sequence: Vec<u8> = Vec::new();
    fasta
        .read(&mut ref_sequence)
        .expect("failed to read fasta sequence");
    //if pos == 69505 { println!("yes, this one"); }
    let mut alt_sequence: Vec<u8> = Vec::new();
    for i in 0..window as usize {
        alt_sequence.push(ref_sequence[i]);
    }
    for base in alt_allele.as_bytes() {
        alt_sequence.push(*base);
    }
    for i in (window as usize + ref_allele.len())..ref_sequence.len() {
        alt_sequence.push(ref_sequence[i]);
    }
    let mut read_names_ref: Vec<String> = Vec::new();
    let mut read_names_alt: Vec<String> = Vec::new();
    for _rec in bam.records() {
        let rec = _rec.expect("cannot read bam record");
        if rec.mapq() < min_mapq {
            continue;
        }
        if rec.is_secondary() || rec.is_supplementary() {
            continue;
        }
        let mut read_start: Option<usize> = None; // I am lazy and for some reason dont know how to do things, so this is my bad solution
        let mut read_end: usize = rec.seq_len();
        let mut min_bq = 93;
        let qual = rec.qual();
        for pos_pair in rec.aligned_pairs() {
            if (pos_pair[1] as u64) >= ref_start && (pos_pair[1] as u64) < ref_end {
                if pos_pair[1] as usize >= pos
                    && pos_pair[1] as usize <= pos + ref_allele.len().max(alt_allele.len())
                {
                    min_bq = min_bq.min(qual[pos_pair[0] as usize]);
                }
                read_end = pos_pair[0] as usize;
                if read_start == None {
                    read_start = Some(pos_pair[0] as usize); // assign this value only the first time in this loop that outter if statement is true
                                                             // getting the position in the read that corresponds to the reference region of pos-window
                }
            }
        }

        if min_bq < min_base_qual {
            continue;
        }
        if read_start == None {
            //println!(
            //    "what happened, read start {:?} read end {}",
            //    read_start, read_end
            //);
            continue;
        }
        let read_start = read_start.expect("why read start is none");
        let seq = rec.seq().as_bytes()[read_start..read_end].to_vec();
        let score = |a: u8, b: u8| if a == b { MATCH } else { MISMATCH };
        let mut aligner = banded::Aligner::new(GAP_OPEN, GAP_EXTEND, score, K, W);
        let ref_alignment = aligner.local(&seq, &ref_sequence);
        let alt_alignment = aligner.local(&seq, &alt_sequence);
        if ref_alignment.score > alt_alignment.score {
            read_names_ref.push(std::str::from_utf8(rec.qname()).expect("wtff").replace(":","_").replace(";","-").to_string());
        } else if alt_alignment.score > ref_alignment.score {
            read_names_alt.push(std::str::from_utf8(rec.qname()).expect("wtf").replace(":","_").replace(";","-").to_string());
        }
        //if pos == 69505 { println!("scores {} and {}",ref_alignment.score, alt_alignment.score);
        //println!("read_names_ref {}, read_names_alt {}", read_names_ref.len(), read_names_alt.len()); }
    }

    let concat_ref = read_names_ref.join(";");
    let concat_alt = read_names_alt.join(";");
    (concat_ref, concat_alt)
}

fn is_heterozygous(gt: bcf::record::Genotype) -> bool {
    if gt[0] == bcf::record::GenotypeAllele::Unphased(0)
        && gt[1] == bcf::record::GenotypeAllele::Unphased(1)
    {
        return true;
    } else if gt[0] == bcf::record::GenotypeAllele::Unphased(1)
        && gt[1] == bcf::record::GenotypeAllele::Unphased(0)
    {
        return true;
    } else if gt[0] == bcf::record::GenotypeAllele::Unphased(0)
        && gt[1] == bcf::record::GenotypeAllele::Phased(1)
    {
        return true;
    } else if gt[0] == bcf::record::GenotypeAllele::Unphased(1)
        && gt[1] == bcf::record::GenotypeAllele::Phased(0)
    {
        return true;
    }
    return false;
}

struct ThreadData {
    index: usize,
    long_read_bam: Option<String>,
    hic_bam: Option<String>,
    fasta: String,
    chrom: String,
    chrom_length: u64,
    vcf: String,
    min_mapq: u8,
    min_base_qual: u8,
    window: usize,
    output: String,
    vcf_out: String,
    vcf_out_done: String,
    phasing_window: usize,
    seed: u8,
    ploidy: usize,
    hic_phasing_posterior_threshold: f32,
    long_switch_threshold: f32,
}

#[derive(Clone)]
struct Params {
    hic_bam: Option<String>,
    long_read_bam: Option<String>,
    output: String,
    seed: u8,
    threads: usize,
    fasta: String,
    ploidy: usize,
    vcf: String,
    min_mapq: u8,
    min_base_qual: u8,
    window: usize,
    phasing_window: usize,
    hic_phasing_posterior_threshold: f32,
    long_switch_threshold: f32,
}

fn load_params() -> Params {
    let yaml = load_yaml!("params.yml");
    let params = App::from_yaml(yaml).get_matches();

    let output = params.value_of("output").unwrap();
    let hic_bam = match params.value_of("hic_bam") {
        Some(x) => Some(x.to_string()),
        None => None,
    };
    let long_read_bam = match params.value_of("long_read_bam") {
        Some(x) => Some(x.to_string()),
        None => None,
    };

    let threads = params.value_of("threads").unwrap();
    let threads = threads.to_string().parse::<usize>().unwrap();

    let seed = params.value_of("seed").unwrap();
    let seed = seed.to_string().parse::<u8>().unwrap();

    let min_mapq = params.value_of("min_mapq").unwrap();
    let min_mapq = min_mapq.to_string().parse::<u8>().unwrap();

    let min_base_qual = params.value_of("min_base_qual").unwrap();
    let min_base_qual = min_base_qual.to_string().parse::<u8>().unwrap();

    let hic_phasing_posterior_threshold = params.value_of("hic_phasing_posterior_threshold").unwrap();
    let hic_phasing_posterior_threshold = hic_phasing_posterior_threshold.to_string().parse::<f32>().unwrap();


    let fasta = params.value_of("fasta").unwrap();

    let ploidy = params.value_of("ploidy").unwrap();
    let ploidy = ploidy.to_string().parse::<usize>().unwrap();
    assert!(ploidy != 1, "what are you doing trying to phase a haploid genome, ploidy can't be 1 here");

    let allele_alignment_window = params.value_of("allele_alignment_window").unwrap();
    let allele_alignment_window = allele_alignment_window
        .to_string()
        .parse::<usize>()
        .unwrap();

    let phasing_window = params.value_of("phasing_window").unwrap();
    let phasing_window = phasing_window.to_string().parse::<usize>().unwrap();

    let long_switch_threshold = params.value_of("long_switch_threshold").unwrap();
    let long_switch_threshold = long_switch_threshold.to_string().parse::<f32>().unwrap();

    let vcf = params.value_of("vcf").unwrap();

    Params {
        output: output.to_string(),
        long_read_bam: long_read_bam,
        hic_bam: hic_bam,
        threads: threads,
        seed: seed,
        fasta: fasta.to_string(),
        ploidy: ploidy,
        vcf: vcf.to_string(),
        min_mapq: min_mapq,
        min_base_qual: min_base_qual,
        window: allele_alignment_window,
        phasing_window: phasing_window,
        hic_phasing_posterior_threshold: hic_phasing_posterior_threshold,
        long_switch_threshold: long_switch_threshold,
    }
}
