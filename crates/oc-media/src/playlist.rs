//! Playing one file after another (`MainWindow._play_next` in 1.x).
//!
//! When a file ends ([`crate::Event::Finished`]), the host selects the next
//! file *after the cursor* that the player plays (other files in between are
//! skipped) and starts it even with autoplay off. It does nothing when:
//! - the user moved the cursor away from the file that ended (the cursor
//!   stays where they put it), or the side no longer shows the preview;
//! - the file was the last playable one in the folder (playback stops).
//!
//! With "Play the next file when one ends" off (Settings → Preview,
//! `Settings.play_next`, default on), videos repeat until another file is
//! selected and audio stops at its end ([`should_loop`]).

/// ← / → move this far (ms).
pub const SEEK_STEP_MS: i64 = 5000;

/// Whether the player should repeat the file: only videos, and only when the
/// host doesn't move on to the next file.
pub fn should_loop(is_video: bool, play_next: bool) -> bool {
    is_video && !play_next
}

/// The index of the first item after `current` that `plays` accepts.
pub fn next_to_play<T>(items: &[T], current: usize, mut plays: impl FnMut(&T) -> bool) -> Option<usize> {
    items.iter().enumerate().skip(current.saturating_add(1)).find(|(_, item)| plays(item)).map(|(i, _)| i)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skips_other_files_and_stops_at_the_end() {
        let names = ["..", "1.mp3", "2.mp3", "notes.txt", "3.mp3"];
        let plays = |n: &&str| n.ends_with(".mp3");
        assert_eq!(next_to_play(&names, 2, plays), Some(4));
        assert_eq!(next_to_play(&names, 4, plays), None);
        assert_eq!(next_to_play(&names, usize::MAX, plays), None);
    }

    #[test]
    fn videos_loop_only_without_play_next() {
        assert!(should_loop(true, false));
        assert!(!should_loop(true, true));
        assert!(!should_loop(false, false));
    }
}
