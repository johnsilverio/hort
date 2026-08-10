//! The host address of a project's dependency cache. A pure, reversible
//! encoding of the project path into one directory name, so a later run can ask
//! which project a stored cache belongs to and whether that project is still
//! there.

use std::path::{Path, PathBuf};

const ESCAPE: char = '%';
const ESCAPED_ESCAPE: &str = "%25";
const ESCAPED_SEPARATOR: &str = "%2F";

/// The directory name a project's caches live under, encoded so the project it
/// belongs to can be read back out of it.
pub fn project_cache_key(project_dir: &Path) -> String {
    project_dir
        .display()
        .to_string()
        // The escape is spent on itself first. The other way round, a project
        // path already holding one would come out wearing the name of a
        // different project, and the decoding would hand back that other one.
        .replace(ESCAPE, ESCAPED_ESCAPE)
        .replace('/', ESCAPED_SEPARATOR)
}

/// The project a cache directory name was made from.
pub fn project_from_cache_key(key: &str) -> PathBuf {
    let mut project = String::with_capacity(key.len());
    let mut rest = key;
    while let Some(escape) = rest.find(ESCAPE) {
        project.push_str(&rest[..escape]);
        let (decoded, width) = match &rest[escape..] {
            tail if tail.starts_with(ESCAPED_SEPARATOR) => ('/', ESCAPED_SEPARATOR.len()),
            tail if tail.starts_with(ESCAPED_ESCAPE) => (ESCAPE, ESCAPED_ESCAPE.len()),
            _ => (ESCAPE, ESCAPE.len_utf8()),
        };
        project.push(decoded);
        rest = &rest[escape + width..];
    }
    project.push_str(rest);
    PathBuf::from(project)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_project_cache_key_is_one_directory_name() {
        let key = project_cache_key(Path::new("/home/tester/projects/hort"));

        // The caches of one project sit side by side under a single directory,
        // and a key that kept the separators would spread them down a tree
        // shaped like the user's own disk, where nothing could tell the
        // directory that stands for a project from the ones inside it.
        assert_eq!(key, "%2Fhome%2Ftester%2Fprojects%2Fhort");
    }

    #[test]
    fn a_cache_host_path_can_be_decoded_back_to_its_project() {
        // A project path already holding the escape is the one case that tells
        // the two substitutions apart: encoded the other way round, this project
        // would end up wearing the same name as /home/tester/projects/a/b.
        let project = Path::new("/home/tester/projects/a%2Fb");

        let decoded = project_from_cache_key(&project_cache_key(project));

        // Reversible is the whole reason this is an encoding and not a hash: a
        // cache directory has to be able to say which project it belongs to,
        // and the only reader that will ever ask is one running long after the
        // project may have been deleted.
        assert_eq!(decoded, project);
    }
}
