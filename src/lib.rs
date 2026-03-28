use zed_extension_api as zed;

struct GitNotesExtension;

impl zed::Extension for GitNotesExtension {
    fn new() -> Self {
        GitNotesExtension
    }
}

zed::register_extension!(GitNotesExtension);
