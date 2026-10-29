use std::io::Read;

#[derive(Debug)]
pub(crate) enum BoundedReadError {
    TooLarge { limit: usize },
    Io(std::io::Error),
}

pub(crate) fn read_bounded(
    reader: impl Read,
    limit: usize,
    size_hint: u64,
) -> Result<Vec<u8>, BoundedReadError> {
    let hint = usize::try_from(size_hint).unwrap_or_default().min(limit);
    let mut bytes = Vec::with_capacity(hint);
    let cap = u64::try_from(limit).unwrap_or(u64::MAX).saturating_add(1);
    reader
        .take(cap)
        .read_to_end(&mut bytes)
        .map_err(BoundedReadError::Io)?;
    if bytes.len() > limit {
        return Err(BoundedReadError::TooLarge { limit });
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_up_to_the_limit_and_refuses_one_byte_over() {
        assert_eq!(read_bounded(&b"abc"[..], 3, 3).unwrap(), b"abc");
        assert_eq!(read_bounded(&b"abc"[..], 8, 0).unwrap(), b"abc");
        let error = read_bounded(&b"abcd"[..], 3, 4).unwrap_err();
        assert!(matches!(error, BoundedReadError::TooLarge { limit: 3 }));
    }

    #[test]
    fn stops_reading_one_byte_past_the_limit() {
        struct Counting<'a>(&'a [u8], usize);
        impl Read for Counting<'_> {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                let read = self.0.read(buffer)?;
                self.1 += read;
                Ok(read)
            }
        }
        let mut source = Counting(&[0_u8; 1024][..], 0);
        let error = read_bounded(&mut source, 16, 1024).unwrap_err();
        assert!(matches!(error, BoundedReadError::TooLarge { limit: 16 }));
        assert_eq!(source.1, 17);
    }

    #[test]
    fn a_hint_beyond_the_limit_does_not_reserve_beyond_it() {
        let bytes = read_bounded(&b"ab"[..], 4, u64::MAX).unwrap();
        assert!(bytes.capacity() <= 4 + 1);
    }
}
