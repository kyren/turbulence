use std::{
    alloc::{alloc, dealloc, Layout},
    mem::{self, MaybeUninit},
    ptr::NonNull,
    slice,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

use cache_padded::CachePadded;

struct RingBuffer {
    buffer: NonNull<MaybeUninit<u8>>,
    capacity: usize,
    head: CachePadded<AtomicUsize>,
    tail: CachePadded<AtomicUsize>,
}

impl RingBuffer {
    fn new(capacity: usize) -> Self {
        assert!(capacity != 0);
        Self {
            buffer: unsafe {
                NonNull::new(alloc(Layout::array::<MaybeUninit<u8>>(capacity).unwrap())
                    as *mut MaybeUninit<u8>)
                .unwrap()
            },
            capacity,
            head: CachePadded::new(AtomicUsize::new(0)),
            tail: CachePadded::new(AtomicUsize::new(0)),
        }
    }
}

impl Drop for RingBuffer {
    fn drop(&mut self) {
        unsafe {
            dealloc(
                self.buffer.as_ptr() as *mut u8,
                Layout::array::<MaybeUninit<u8>>(self.capacity).unwrap(),
            );
        }
    }
}

unsafe impl Send for RingBuffer {}
unsafe impl Sync for RingBuffer {}

pub fn ring_buffer(capacity: usize) -> (Writer, Reader) {
    let buffer = Arc::new(RingBuffer::new(capacity));
    let writer = Writer(buffer.clone());
    let reader = Reader(buffer);
    (writer, reader)
}

pub struct Writer(Arc<RingBuffer>);

impl Writer {
    pub fn available(&self) -> usize {
        let head = self.0.head.load(Ordering::Acquire);
        let tail = self.0.tail.load(Ordering::Acquire);

        head_to_tail(self.0.capacity, head, tail)
    }

    pub fn write(&mut self, mut data: &[u8]) -> usize {
        let head_pos = self.0.head.load(Ordering::Acquire);
        let tail_pos = self.0.tail.load(Ordering::Acquire);

        let head = collapse_position(self.0.capacity, head_pos);
        let tail = collapse_position(self.0.capacity, tail_pos);

        if head == tail && head_pos != tail_pos {
            return 0;
        }

        let (left, right): (&mut [MaybeUninit<u8>], &mut [MaybeUninit<u8>]) = unsafe {
            if head < tail {
                (
                    slice::from_raw_parts_mut(self.0.buffer.as_ptr().add(head), tail - head),
                    &mut [],
                )
            } else {
                (
                    slice::from_raw_parts_mut(
                        self.0.buffer.as_ptr().add(head),
                        self.0.capacity - head,
                    ),
                    slice::from_raw_parts_mut(self.0.buffer.as_ptr(), tail),
                )
            }
        };

        let left_len = left.len().min(data.len());
        write_slice(&mut left[0..left_len], &data[0..left_len]);
        data = &data[left_len..];

        let right_len = right.len().min(data.len());
        write_slice(&mut right[0..right_len], &data[0..right_len]);

        let written = left_len + right_len;
        self.0.head.store(
            increment(self.0.capacity, head_pos, written),
            Ordering::Release,
        );

        written
    }
}

pub struct Reader(Arc<RingBuffer>);

impl Reader {
    pub fn available(&self) -> usize {
        let head = self.0.head.load(Ordering::Acquire);
        let tail = self.0.tail.load(Ordering::Acquire);

        tail_to_head(self.0.capacity, tail, head)
    }

    pub fn peek(&self, mut offset: usize, mut data: &mut [u8]) -> usize {
        let head_pos = self.0.head.load(Ordering::Acquire);
        let tail_pos = self.0.tail.load(Ordering::Acquire);

        let head = collapse_position(self.0.capacity, head_pos);
        let tail = collapse_position(self.0.capacity, tail_pos);

        if head == tail && head_pos == tail_pos {
            return 0;
        }

        let (mut left, mut right): (&[u8], &[u8]) = unsafe {
            if tail < head {
                (
                    slice::from_raw_parts(self.0.buffer.as_ptr().add(tail) as *mut u8, head - tail),
                    &mut [],
                )
            } else {
                (
                    slice::from_raw_parts(
                        self.0.buffer.as_ptr().add(tail) as *mut u8,
                        self.0.capacity - tail,
                    ),
                    slice::from_raw_parts(self.0.buffer.as_ptr() as *mut u8, head),
                )
            }
        };

        let left_eat = left.len().min(offset);
        left = &left[left_eat..];
        offset -= left_eat;

        let left_len = left.len().min(data.len());
        data[0..left_len].copy_from_slice(&left[0..left_len]);
        data = &mut data[left_len..];

        let right_eat = right.len().min(offset);
        right = &right[right_eat..];

        let right_len = right.len().min(data.len());
        data[0..right_len].copy_from_slice(&right[0..right_len]);

        left_len + right_len
    }

    pub fn advance(&mut self, offset: usize) -> usize {
        let head = self.0.head.load(Ordering::Acquire);
        let tail = self.0.tail.load(Ordering::Acquire);

        let offset = offset.min(tail_to_head(self.0.capacity, tail, head));
        let tail = increment(self.0.capacity, tail, offset);
        self.0.tail.store(tail, Ordering::Release);

        offset
    }
}

fn collapse_position(capacity: usize, pos: usize) -> usize {
    if pos < capacity {
        pos
    } else {
        pos - capacity
    }
}

fn tail_to_head(capacity: usize, tail: usize, head: usize) -> usize {
    if tail <= head {
        head - tail
    } else {
        capacity - (tail - capacity) + head
    }
}

fn head_to_tail(capacity: usize, head: usize, tail: usize) -> usize {
    capacity - tail_to_head(capacity, tail, head)
}

fn increment(capacity: usize, pos: usize, n: usize) -> usize {
    if n == 0 {
        return pos;
    }

    let threshold = (capacity - n) + capacity;
    if pos < threshold {
        pos + n
    } else {
        pos - threshold
    }
}

fn write_slice(dst: &mut [MaybeUninit<u8>], src: &[u8]) {
    let src: &[MaybeUninit<u8>] = unsafe { mem::transmute(src) };
    dst.copy_from_slice(src);
}

#[cfg(test)]
mod tests {
    use std::{thread, time::Duration};

    use super::*;

    #[test]
    fn basic_read_write() {
        let (mut writer, mut reader) = ring_buffer(7);
        let mut buffer = [0; 7];

        assert_eq!(writer.available(), 7);
        assert_eq!(writer.write(&[0, 1, 2]), 3);
        assert_eq!(writer.available(), 4);
        assert_eq!(reader.available(), 3);
        assert_eq!(reader.peek(0, &mut buffer), 3);
        assert_eq!(buffer[0..3], [0, 1, 2]);
        assert_eq!(writer.available(), 4);
        assert_eq!(reader.advance(3), 3);
        assert_eq!(writer.available(), 7);
        assert_eq!(reader.available(), 0);
        assert_eq!(writer.write(&[0, 1, 2]), 3);
        assert_eq!(writer.available(), 4);
        assert_eq!(reader.peek(0, &mut buffer[0..3]), 3);
        assert_eq!(buffer[0..3], [0, 1, 2]);
        assert_eq!(writer.write(&[3, 4, 5]), 3);
        assert_eq!(writer.available(), 1);
        assert_eq!(writer.write(&[6, 7, 8, 9]), 1);
        assert_eq!(writer.available(), 0);
        assert_eq!(reader.available(), 7);
        assert_eq!(reader.peek(4, &mut buffer[0..5]), 3);
        assert_eq!(buffer[0..3], [4, 5, 6]);
        assert_eq!(reader.peek(0, &mut buffer[0..2]), 2);
        assert_eq!(buffer[0..2], [0, 1]);
        assert_eq!(reader.advance(2), 2);
        assert_eq!(reader.available(), 5);
        assert_eq!(writer.available(), 2);
        assert_eq!(reader.peek(0, &mut buffer[0..3]), 3);
        assert_eq!(buffer[0..3], [2, 3, 4]);
        assert_eq!(reader.advance(3), 3);
        assert_eq!(reader.available(), 2);
        assert_eq!(writer.available(), 5);
        assert_eq!(reader.peek(0, &mut buffer[0..5]), 2);
        assert_eq!(buffer[0..2], [5, 6]);
        assert_eq!(reader.available(), 2);
        assert_eq!(writer.available(), 5);
        assert_eq!(reader.advance(5), 2);
        assert_eq!(reader.available(), 0);
        assert_eq!(writer.available(), 7);
        assert_eq!(writer.write(&[0, 1, 2, 3, 4]), 5);
        assert_eq!(writer.available(), 2);
        assert_eq!(reader.available(), 5);
        assert_eq!(reader.peek(2, &mut buffer[0..5]), 3);
        assert_eq!(buffer[0..3], [2, 3, 4]);
    }

    #[test]
    fn threaded_read_write() {
        let (mut writer, mut reader) = ring_buffer(64);

        let a = thread::spawn(move || {
            let mut b = [0; 32];
            let mut i = 0;
            loop {
                let write = 11 + (i % 17);
                for j in 0..write {
                    b[j] = ((i + j) % 256) as u8;
                }
                i += writer.write(&b[0..write]);
                if i >= 10_000 {
                    break;
                }
            }
        });

        let b = thread::spawn(move || {
            let mut b = [0; 32];
            let mut i = 0;
            loop {
                let r = reader.peek(0, &mut b);
                for j in 0..r {
                    assert_eq!(b[j], ((i + j) % 256) as u8);
                }
                assert_eq!(reader.advance(r), r);
                i += r;
                if i >= 10_000 {
                    break;
                }
            }
        });

        b.join().unwrap();
        a.join().unwrap();
    }
}
