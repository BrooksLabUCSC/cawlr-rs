use std::{io, path::Path};

pub fn is_running_in_container() -> io::Result<bool> {
    Path::new("/.dockerenv").try_exists()
}
