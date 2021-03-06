use crate::common::Common;
use crate::common_directory::Directories;
use crate::common_items::ExcludedItems;
use crate::common_messages::Messages;
use crate::common_traits::{DebugPrint, PrintResults, SaveResults};
use bk_tree::BKTree;
use crossbeam_channel::Receiver;
use humansize::{file_size_opts as options, FileSize};
use image::GenericImageView;
use img_hash::HasherConfig;
use rayon::prelude::*;
use std::collections::HashMap;
use std::fs;
use std::fs::{File, Metadata};
use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Eq, PartialEq, Ord, PartialOrd, Debug)]
pub enum Similarity {
    None,
    VerySmall,
    Small,
    Medium,
    High,
    VeryHigh,
}

#[derive(Clone)]
pub struct FileEntry {
    pub path: PathBuf,
    pub size: u64,
    pub dimensions: String,
    pub modified_date: u64,
    pub similarity: Similarity,
}

/// Type to store for each entry in the similarity BK-tree.
type Node = [u8; 8];

/// Distance metric to use with the BK-tree.
struct Hamming;

impl bk_tree::Metric<Node> for Hamming {
    fn distance(&self, a: &Node, b: &Node) -> u64 {
        hamming::distance_fast(a, b).unwrap()
    }
}

/// Struct to store most basics info about all folder
pub struct SimilarImages {
    information: Info,
    text_messages: Messages,
    directories: Directories,
    excluded_items: ExcludedItems,
    bktree: BKTree<Node, Hamming>,
    similar_vectors: Vec<Vec<FileEntry>>,
    recursive_search: bool,
    minimal_file_size: u64,
    image_hashes: HashMap<Node, Vec<FileEntry>>, // Hashmap with image hashes and Vector with names of files
    stopped_search: bool,
    similarity: Similarity,
    images_to_check: Vec<FileEntry>,
}

/// Info struck with helpful information's about results
#[derive(Default)]
pub struct Info {
    pub number_of_checked_files: usize,
    pub number_of_checked_folders: usize,
    pub number_of_ignored_files: usize,
    pub number_of_ignored_things: usize,
    pub size_of_checked_images: u64,
    pub lost_space: u64,
    pub number_of_removed_files: usize,
    pub number_of_failed_to_remove_files: usize,
    pub gained_space: u64,
}
impl Info {
    pub fn new() -> Self {
        Default::default()
    }
}

/// Method implementation for EmptyFolder
impl SimilarImages {
    /// New function providing basics values
    pub fn new() -> Self {
        Self {
            information: Default::default(),
            text_messages: Messages::new(),
            directories: Directories::new(),
            excluded_items: Default::default(),
            bktree: BKTree::new(Hamming),
            similar_vectors: vec![],
            recursive_search: true,
            minimal_file_size: 1024 * 16, // 16 KB should be enough to exclude too small images from search
            image_hashes: Default::default(),
            stopped_search: false,
            similarity: Similarity::High,
            images_to_check: vec![],
        }
    }

    pub fn get_stopped_search(&self) -> bool {
        self.stopped_search
    }

    pub const fn get_text_messages(&self) -> &Messages {
        &self.text_messages
    }

    pub const fn get_similar_images(&self) -> &Vec<Vec<FileEntry>> {
        &self.similar_vectors
    }

    pub const fn get_information(&self) -> &Info {
        &self.information
    }

    pub fn set_recursive_search(&mut self, recursive_search: bool) {
        self.recursive_search = recursive_search;
    }

    pub fn set_minimal_file_size(&mut self, minimal_file_size: u64) {
        self.minimal_file_size = match minimal_file_size {
            0 => 1,
            t => t,
        };
    }
    pub fn set_similarity(&mut self, similarity: Similarity) {
        self.similarity = similarity;
    }

    /// Public function used by CLI to search for empty folders
    pub fn find_similar_images(&mut self, stop_receiver: Option<&Receiver<()>>) {
        self.directories.optimize_directories(true, &mut self.text_messages);
        if !self.check_for_similar_images(stop_receiver) {
            self.stopped_search = true;
            return;
        }
        if !self.sort_images(stop_receiver) {
            self.stopped_search = true;
            return;
        }
        // if self.delete_folders {
        //     self.delete_empty_folders();
        // }
        self.debug_print();
    }

    // pub fn set_delete_folder(&mut self, delete_folder: bool) {
    //     self.delete_folders = delete_folder;
    // }

    /// Function to check if folder are empty.
    /// Parameter initial_checking for second check before deleting to be sure that checked folder is still empty
    fn check_for_similar_images(&mut self, stop_receiver: Option<&Receiver<()>>) -> bool {
        let start_time: SystemTime = SystemTime::now();
        let mut folders_to_check: Vec<PathBuf> = Vec::with_capacity(1024 * 2); // This should be small enough too not see to big difference and big enough to store most of paths without needing to resize vector

        // Add root folders for finding
        for id in &self.directories.included_directories {
            folders_to_check.push(id.clone());
        }
        self.information.number_of_checked_folders += folders_to_check.len();

        while !folders_to_check.is_empty() {
            if stop_receiver.is_some() && stop_receiver.unwrap().try_recv().is_ok() {
                return false;
            }
            let current_folder = folders_to_check.pop().unwrap();

            // Read current dir, if permission are denied just go to next
            let read_dir = match fs::read_dir(&current_folder) {
                Ok(t) => t,
                Err(_) => {
                    self.text_messages.warnings.push(format!("Cannot open dir {}", current_folder.display()));
                    continue;
                } // Permissions denied
            };

            // Check every sub folder/file/link etc.
            'dir: for entry in read_dir {
                let entry_data = match entry {
                    Ok(t) => t,
                    Err(_) => {
                        self.text_messages.warnings.push(format!("Cannot read entry in dir {}", current_folder.display()));
                        continue;
                    } //Permissions denied
                };
                let metadata: Metadata = match entry_data.metadata() {
                    Ok(t) => t,
                    Err(_) => {
                        self.text_messages.warnings.push(format!("Cannot read metadata in dir {}", current_folder.display()));
                        continue;
                    } //Permissions denied
                };
                if metadata.is_dir() {
                    self.information.number_of_checked_folders += 1;

                    if !self.recursive_search {
                        continue;
                    }

                    let next_folder = current_folder.join(entry_data.file_name());
                    if self.directories.is_excluded(&next_folder) {
                        continue 'dir;
                    }

                    if self.excluded_items.is_excluded(&next_folder) {
                        continue 'dir;
                    }

                    folders_to_check.push(next_folder);
                } else if metadata.is_file() {
                    // let mut have_valid_extension: bool;
                    let file_name_lowercase: String = match entry_data.file_name().into_string() {
                        Ok(t) => t,
                        Err(_) => continue,
                    }
                    .to_lowercase();

                    // Checking allowed image extensions
                    let allowed_image_extensions = ["jpg", "png", "bmp"];
                    if !allowed_image_extensions.iter().any(|e| file_name_lowercase.ends_with(e)) {
                        self.information.number_of_ignored_files += 1;
                        continue 'dir;
                    }

                    // Checking files
                    if metadata.len() >= self.minimal_file_size {
                        let current_file_name = current_folder.join(entry_data.file_name());
                        if self.excluded_items.is_excluded(&current_file_name) {
                            continue 'dir;
                        }

                        let fe: FileEntry = FileEntry {
                            path: current_file_name.clone(),
                            size: metadata.len(),
                            dimensions: "".to_string(),
                            modified_date: match metadata.modified() {
                                Ok(t) => match t.duration_since(UNIX_EPOCH) {
                                    Ok(d) => d.as_secs(),
                                    Err(_) => {
                                        self.text_messages.warnings.push(format!("File {} seems to be modified before Unix Epoch.", current_file_name.display()));
                                        0
                                    }
                                },
                                Err(_) => {
                                    self.text_messages.warnings.push(format!("Unable to get modification date from file {}", current_file_name.display()));
                                    continue;
                                } // Permissions Denied
                            },

                            similarity: Similarity::None,
                        };

                        self.images_to_check.push(fe);

                        self.information.size_of_checked_images += metadata.len();
                        self.information.number_of_checked_files += 1;
                    } else {
                        // Probably this is symbolic links so we are free to ignore this
                        self.information.number_of_ignored_files += 1;
                    }
                } else {
                    // Probably this is symbolic links so we are free to ignore this
                    self.information.number_of_ignored_things += 1;
                }
            }
        }
        Common::print_time(start_time, SystemTime::now(), "check_for_similar_images".to_string());
        true
    }

    fn sort_images(&mut self, stop_receiver: Option<&Receiver<()>>) -> bool {
        let hash_map_modification = SystemTime::now();

        let vec_file_entry: Vec<(FileEntry, [u8; 8])> = self
            .images_to_check
            .par_iter()
            .map(|file_entry| {
                if stop_receiver.is_some() && stop_receiver.unwrap().try_recv().is_ok() {
                    // This will not break
                    return None;
                }
                let mut file_entry = file_entry.clone();

                let image = match image::open(file_entry.path.clone()) {
                    Ok(t) => t,
                    Err(_) => return Some(None), // Something is wrong with image
                };
                let dimensions = image.dimensions();

                file_entry.dimensions = format!("{}x{}", dimensions.0, dimensions.1);
                let hasher = HasherConfig::with_bytes_type::<[u8; 8]>().to_hasher();

                let hash = hasher.hash_image(&image);
                let mut buf = [0u8; 8];
                buf.copy_from_slice(&hash.as_bytes());

                Some(Some((file_entry, buf)))
            })
            .while_some()
            .filter(|file_entry| file_entry.is_some())
            .map(|file_entry| file_entry.unwrap())
            .collect::<Vec<(FileEntry, [u8; 8])>>();

        for (file_entry, buf) in vec_file_entry {
            self.bktree.add(buf);
            self.image_hashes.entry(buf).or_insert_with(Vec::<FileEntry>::new);
            self.image_hashes.get_mut(&buf).unwrap().push(file_entry.clone());
        }

        Common::print_time(hash_map_modification, SystemTime::now(), "sort_images - reading data from files".to_string());
        let hash_map_modification = SystemTime::now();

        let similarity: u64 = match self.similarity {
            Similarity::VeryHigh => 0,
            Similarity::High => 1,
            Similarity::Medium => 2,
            Similarity::Small => 3,
            Similarity::VerySmall => 4,
            _ => panic!("0-4 similarity levels are allowed, check if not added more."),
        };

        // TODO
        // Now is A is similar to B with VeryHigh and C with Medium
        // And D is similar with C with High
        // And Similarity is set to Medium(or lower)
        // And A is checked before D
        // Then C is shown that is similar group A, not D

        let mut new_vector: Vec<Vec<FileEntry>> = Vec::new();
        let mut hashes_to_check = self.image_hashes.clone();
        for (hash, vec_file_entry) in &self.image_hashes {
            if stop_receiver.is_some() && stop_receiver.unwrap().try_recv().is_ok() {
                return false;
            }
            if !hashes_to_check.contains_key(hash) {
                continue;
            }
            hashes_to_check.remove(hash);

            let vector_with_found_similar_hashes = self.bktree.find(hash, similarity).collect::<Vec<_>>();
            if vector_with_found_similar_hashes.len() == 1 && vec_file_entry.len() == 1 {
                // This one picture doesn't have similar pictures, so there is no go
                continue;
            }

            let mut vector_of_similar_images: Vec<FileEntry> = vec_file_entry
                .iter()
                .map(|fe| FileEntry {
                    path: fe.path.clone(),
                    size: fe.size,
                    dimensions: fe.dimensions.clone(),
                    modified_date: fe.modified_date,
                    similarity: Similarity::VeryHigh,
                })
                .collect();

            for (similarity, similar_hash) in vector_with_found_similar_hashes.iter() {
                if *similarity == 0 && hash == *similar_hash {
                    // This was already read before
                    continue;
                } else if hash == *similar_hash {
                    panic!("I'm not sure if same hash can have distance > 0");
                }

                if let Some(vec_file_entry) = hashes_to_check.get(*similar_hash) {
                    vector_of_similar_images.append(
                        &mut (vec_file_entry
                            .iter()
                            .map(|fe| FileEntry {
                                path: fe.path.clone(),
                                size: fe.size,
                                dimensions: fe.dimensions.clone(),
                                modified_date: fe.modified_date,
                                similarity: match similarity {
                                    0 => Similarity::VeryHigh,
                                    1 => Similarity::High,
                                    2 => Similarity::Medium,
                                    3 => Similarity::Small,
                                    4 => Similarity::VerySmall,
                                    _ => panic!("0-4 similarity levels are allowed, check if not added more."),
                                },
                            })
                            .collect::<Vec<_>>()),
                    );
                    hashes_to_check.remove(*similar_hash);
                }
            }
            if vector_of_similar_images.len() > 1 {
                // Not sure why it may happens
                new_vector.push((*vector_of_similar_images).to_owned());
            }
        }

        self.similar_vectors = new_vector;

        Common::print_time(hash_map_modification, SystemTime::now(), "sort_images - selecting data from BtreeMap".to_string());
        true
    }

    /// Set included dir which needs to be relative, exists etc.
    pub fn set_included_directory(&mut self, included_directory: String) {
        self.directories.set_included_directory(included_directory, &mut self.text_messages);
    }

    pub fn set_excluded_directory(&mut self, excluded_directory: String) {
        self.directories.set_excluded_directory(excluded_directory, &mut self.text_messages);
    }

    pub fn set_excluded_items(&mut self, excluded_items: String) {
        self.excluded_items.set_excluded_items(excluded_items, &mut self.text_messages);
    }
}
impl Default for SimilarImages {
    fn default() -> Self {
        Self::new()
    }
}

impl DebugPrint for SimilarImages {
    #[allow(dead_code)]
    #[allow(unreachable_code)]
    fn debug_print(&self) {
        #[cfg(not(debug_assertions))]
        {
            return;
        }

        println!("---------------DEBUG PRINT---------------");
        println!("Number of all checked folders - {}", self.information.number_of_checked_folders);
        println!("Included directories - {:?}", self.directories.included_directories);
        println!("Checked images {} / Different photos {}", self.information.number_of_checked_files, self.image_hashes.len());
        println!(
            "Size of checked images {} ({} Bytes)",
            self.information.size_of_checked_images.file_size(options::BINARY).unwrap(),
            self.information.size_of_checked_images
        );
        println!("-----------------------------------------");
    }
}
impl SaveResults for SimilarImages {
    fn save_results_to_file(&mut self, file_name: &str) -> bool {
        let start_time: SystemTime = SystemTime::now();
        let file_name: String = match file_name {
            "" => "results.txt".to_string(),
            k => k.to_string(),
        };

        let mut file = match File::create(&file_name) {
            Ok(t) => t,
            Err(_) => {
                self.text_messages.errors.push("Failed to create file ".to_string() + file_name.as_str());
                return false;
            }
        };

        if writeln!(
            file,
            "Results of searching {:?} with excluded directories {:?} and excluded items {:?}",
            self.directories.included_directories, self.directories.excluded_directories, self.excluded_items.items
        )
        .is_err()
        {
            self.text_messages.errors.push(format!("Failed to save results to file {}", file_name));
            return false;
        }

        if !self.similar_vectors.is_empty() {
            write!(file, "{} images which have similar friends\n\n", self.similar_vectors.len()).unwrap();

        // for struct_similar in self.similar_vectors.iter() {
        //     writeln!(file, "Image {:?} have {} similar images", struct_similar.base_image.path, struct_similar.similar_images.len()).unwrap();
        //     for similar_picture in struct_similar.similar_images.iter() {
        //         writeln!(file, "{:?} - Similarity Level: {}", similar_picture.path, get_string_from_similarity(&similar_picture.similarity)).unwrap();
        //     }
        //     writeln!(file).unwrap();
        // }
        } else {
            write!(file, "Not found any similar images.").unwrap();
        }
        Common::print_time(start_time, SystemTime::now(), "save_results_to_file".to_string());
        true
    }
}
impl PrintResults for SimilarImages {
    /// Prints basic info about empty folders // TODO print better
    fn print_results(&self) {
        if !self.similar_vectors.is_empty() {
            println!("Found {} images which have similar friends", self.similar_vectors.len());

            for vec_file_entry in &self.similar_vectors {
                for file_entry in vec_file_entry {
                    println!(
                        "{} - {} - {} - {}",
                        file_entry.path.display(),
                        file_entry.dimensions,
                        file_entry.size.file_size(options::BINARY).unwrap(),
                        get_string_from_similarity(&file_entry.similarity)
                    );
                }
                println!();
            }
        }
    }
}

fn get_string_from_similarity(similarity: &Similarity) -> &str {
    match similarity {
        Similarity::VerySmall => "Very Small",
        Similarity::Small => "Small",
        Similarity::Medium => "Medium",
        Similarity::High => "High",
        Similarity::VeryHigh => "Very High",
        Similarity::None => panic!(),
    }
}
