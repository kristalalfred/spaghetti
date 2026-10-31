use coreaudio_sys::{AudioBuffer, AudioBufferList};

/// Claude: `mBuffers` is declared `[AudioBuffer; 1]`, so indexing past 0 through
/// it is out of provenance; derive the buffer pointer from the list's base instead.
#[cfg(target_os = "macos")]
pub unsafe fn buffers<'a>(abl: *const AudioBufferList) -> &'a [AudioBuffer] {
    unsafe {
        let first = abl
            .cast::<u8>()
            .add(std::mem::offset_of!(AudioBufferList, mBuffers))
            .cast::<AudioBuffer>();
        std::slice::from_raw_parts(first, (*abl).mNumberBuffers as usize)
    }
}

pub unsafe fn buffers_mut<'a>(abl: *mut AudioBufferList) -> &'a mut [AudioBuffer] {
    unsafe {
        let first = abl
            .cast::<u8>()
            .add(std::mem::offset_of!(AudioBufferList, mBuffers))
            .cast::<AudioBuffer>();
        std::slice::from_raw_parts_mut(first, (*abl).mNumberBuffers as usize)
    }
}

#[cfg(target_os = "macos")]
pub unsafe fn samples<'a>(buffer: &AudioBuffer) -> Option<&'a [f32]> {
    (!buffer.mData.is_null()).then(|| unsafe {
        std::slice::from_raw_parts(
            buffer.mData as *const f32,
            buffer.mDataByteSize as usize / std::mem::size_of::<f32>(),
        )
    })
}
