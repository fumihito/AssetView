#[cfg(windows)]
mod imp {
    use std::ffi::c_void;
    use std::mem::size_of;
    use std::os::windows::ffi::OsStrExt;
    use std::path::PathBuf;
    use std::ptr::null_mut;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use windows_sys::core::{IID_IUnknown, GUID, HRESULT};
    use windows_sys::Win32::Foundation::{
        GlobalFree, DATA_S_SAMEFORMATETC, DRAGDROP_S_CANCEL, DRAGDROP_S_DROP,
        DRAGDROP_S_USEDEFAULTCURSORS, DV_E_FORMATETC, E_INVALIDARG, E_NOINTERFACE, E_NOTIMPL,
        E_POINTER, S_OK,
    };
    use windows_sys::Win32::System::Com::{DVASPECT_CONTENT, FORMATETC, STGMEDIUM, TYMED_HGLOBAL};
    use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GHND};
    use windows_sys::Win32::System::Ole::{
        DoDragDrop, OleInitialize, OleUninitialize, CF_HDROP, DROPEFFECT_COPY, DROPEFFECT_NONE,
    };
    use windows_sys::Win32::UI::Shell::DROPFILES;

    const IID_IDATAOBJECT: GUID = GUID::from_u128(0x0000010e_0000_0000_c000_000000000046);
    const IID_IDROPSOURCE: GUID = GUID::from_u128(0x00000121_0000_0000_c000_000000000046);
    const MK_LBUTTON: u32 = 0x0001;

    #[repr(C)]
    struct IUnknownVtbl {
        query_interface:
            unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
        add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
        release: unsafe extern "system" fn(*mut c_void) -> u32,
    }

    #[repr(C)]
    struct IDataObjectVtbl {
        parent: IUnknownVtbl,
        get_data:
            unsafe extern "system" fn(*mut c_void, *const FORMATETC, *mut STGMEDIUM) -> HRESULT,
        get_data_here:
            unsafe extern "system" fn(*mut c_void, *const FORMATETC, *mut STGMEDIUM) -> HRESULT,
        query_get_data: unsafe extern "system" fn(*mut c_void, *const FORMATETC) -> HRESULT,
        get_canonical_format_etc:
            unsafe extern "system" fn(*mut c_void, *const FORMATETC, *mut FORMATETC) -> HRESULT,
        set_data: unsafe extern "system" fn(
            *mut c_void,
            *const FORMATETC,
            *const FORMATETC,
            i32,
        ) -> HRESULT,
        enum_format_etc: unsafe extern "system" fn(*mut c_void, u32, *mut *mut c_void) -> HRESULT,
        d_advise: unsafe extern "system" fn(
            *mut c_void,
            *const FORMATETC,
            u32,
            *const c_void,
            *mut u32,
        ) -> HRESULT,
        d_unadvise: unsafe extern "system" fn(*mut c_void, u32) -> HRESULT,
        enum_d_advise: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> HRESULT,
    }

    #[repr(C)]
    struct IDropSourceVtbl {
        parent: IUnknownVtbl,
        query_continue_drag: unsafe extern "system" fn(*mut c_void, i32, u32) -> HRESULT,
        give_feedback: unsafe extern "system" fn(*mut c_void, u32) -> HRESULT,
    }

    #[repr(C)]
    struct FileDataObject {
        vtbl: *const IDataObjectVtbl,
        refcount: AtomicUsize,
        files: Vec<PathBuf>,
    }

    #[repr(C)]
    struct DropSource {
        vtbl: *const IDropSourceVtbl,
        refcount: AtomicUsize,
    }

    impl FileDataObject {
        fn new(files: Vec<PathBuf>) -> Box<Self> {
            Box::new(Self {
                vtbl: &FILE_DATA_OBJECT_VTBL,
                refcount: AtomicUsize::new(1),
                files,
            })
        }
    }

    impl DropSource {
        fn new() -> Box<Self> {
            Box::new(Self {
                vtbl: &DROP_SOURCE_VTBL,
                refcount: AtomicUsize::new(1),
            })
        }
    }

    unsafe fn this_as<T>(this: *mut c_void) -> *mut T {
        this.cast()
    }

    unsafe extern "system" fn data_query_interface(
        this: *mut c_void,
        riid: *const GUID,
        ppv: *mut *mut c_void,
    ) -> HRESULT {
        if ppv.is_null() || riid.is_null() {
            return E_POINTER;
        }
        *ppv = null_mut();
        if guid_eq(&*riid, &IID_IUnknown) || guid_eq(&*riid, &IID_IDATAOBJECT) {
            *ppv = this;
            data_add_ref(this);
            S_OK
        } else {
            E_NOINTERFACE
        }
    }

    unsafe extern "system" fn data_add_ref(this: *mut c_void) -> u32 {
        let obj = this_as::<FileDataObject>(this);
        (unsafe { (*obj).refcount.fetch_add(1, Ordering::Relaxed) } + 1) as u32
    }

    unsafe extern "system" fn data_release(this: *mut c_void) -> u32 {
        let obj = this_as::<FileDataObject>(this);
        let prev = unsafe { (*obj).refcount.fetch_sub(1, Ordering::AcqRel) };
        let count = prev - 1;
        if count == 0 {
            std::sync::atomic::fence(Ordering::Acquire);
            unsafe {
                drop(Box::from_raw(obj));
            }
        }
        count as u32
    }

    unsafe extern "system" fn data_get_data(
        this: *mut c_void,
        format: *const FORMATETC,
        medium: *mut STGMEDIUM,
    ) -> HRESULT {
        if format.is_null() || medium.is_null() {
            return E_POINTER;
        }
        let obj = this_as::<FileDataObject>(this);
        if !supports_hdrop(&*format) {
            return DV_E_FORMATETC;
        }
        match build_hdrop_blob(&(*obj).files) {
            Some(hglobal) => {
                *medium = std::mem::zeroed();
                (*medium).tymed = TYMED_HGLOBAL as u32;
                (*medium).u.hGlobal = hglobal;
                (*medium).pUnkForRelease = null_mut();
                S_OK
            }
            None => E_INVALIDARG,
        }
    }

    unsafe extern "system" fn data_get_data_here(
        _this: *mut c_void,
        _format: *const FORMATETC,
        _medium: *mut STGMEDIUM,
    ) -> HRESULT {
        E_NOTIMPL
    }

    unsafe extern "system" fn data_query_get_data(
        _this: *mut c_void,
        format: *const FORMATETC,
    ) -> HRESULT {
        if format.is_null() {
            return E_POINTER;
        }
        if supports_hdrop(&*format) {
            S_OK
        } else {
            DV_E_FORMATETC
        }
    }

    unsafe extern "system" fn data_get_canonical_format_etc(
        _this: *mut c_void,
        _format_in: *const FORMATETC,
        format_out: *mut FORMATETC,
    ) -> HRESULT {
        if !format_out.is_null() {
            *format_out = std::mem::zeroed();
        }
        DATA_S_SAMEFORMATETC
    }

    unsafe extern "system" fn data_set_data(
        _this: *mut c_void,
        _format: *const FORMATETC,
        _format_out: *const FORMATETC,
        _release: i32,
    ) -> HRESULT {
        E_NOTIMPL
    }

    unsafe extern "system" fn data_enum_format_etc(
        _this: *mut c_void,
        _direction: u32,
        _ppenum: *mut *mut c_void,
    ) -> HRESULT {
        E_NOTIMPL
    }

    unsafe extern "system" fn data_d_advise(
        _this: *mut c_void,
        _format: *const FORMATETC,
        _advf: u32,
        _sink: *const c_void,
        _connection: *mut u32,
    ) -> HRESULT {
        E_NOTIMPL
    }

    unsafe extern "system" fn data_d_unadvise(_this: *mut c_void, _connection: u32) -> HRESULT {
        E_NOTIMPL
    }

    unsafe extern "system" fn data_enum_d_advise(
        _this: *mut c_void,
        _ppenum: *mut *mut c_void,
    ) -> HRESULT {
        E_NOTIMPL
    }

    unsafe extern "system" fn source_query_interface(
        this: *mut c_void,
        riid: *const GUID,
        ppv: *mut *mut c_void,
    ) -> HRESULT {
        if ppv.is_null() || riid.is_null() {
            return E_POINTER;
        }
        *ppv = null_mut();
        if guid_eq(&*riid, &IID_IUnknown) || guid_eq(&*riid, &IID_IDROPSOURCE) {
            *ppv = this;
            source_add_ref(this);
            S_OK
        } else {
            E_NOINTERFACE
        }
    }

    unsafe extern "system" fn source_add_ref(this: *mut c_void) -> u32 {
        let obj = this_as::<DropSource>(this);
        (unsafe { (*obj).refcount.fetch_add(1, Ordering::Relaxed) } + 1) as u32
    }

    unsafe extern "system" fn source_release(this: *mut c_void) -> u32 {
        let obj = this_as::<DropSource>(this);
        let prev = unsafe { (*obj).refcount.fetch_sub(1, Ordering::AcqRel) };
        let count = prev - 1;
        if count == 0 {
            std::sync::atomic::fence(Ordering::Acquire);
            unsafe {
                drop(Box::from_raw(obj));
            }
        }
        count as u32
    }

    unsafe extern "system" fn source_query_continue_drag(
        _this: *mut c_void,
        escape_pressed: i32,
        key_state: u32,
    ) -> HRESULT {
        if escape_pressed != 0 {
            DRAGDROP_S_CANCEL
        } else if key_state & MK_LBUTTON == 0 {
            DRAGDROP_S_DROP
        } else {
            S_OK
        }
    }

    unsafe extern "system" fn source_give_feedback(_this: *mut c_void, _effect: u32) -> HRESULT {
        DRAGDROP_S_USEDEFAULTCURSORS
    }

    static FILE_DATA_OBJECT_VTBL: IDataObjectVtbl = IDataObjectVtbl {
        parent: IUnknownVtbl {
            query_interface: data_query_interface,
            add_ref: data_add_ref,
            release: data_release,
        },
        get_data: data_get_data,
        get_data_here: data_get_data_here,
        query_get_data: data_query_get_data,
        get_canonical_format_etc: data_get_canonical_format_etc,
        set_data: data_set_data,
        enum_format_etc: data_enum_format_etc,
        d_advise: data_d_advise,
        d_unadvise: data_d_unadvise,
        enum_d_advise: data_enum_d_advise,
    };

    static DROP_SOURCE_VTBL: IDropSourceVtbl = IDropSourceVtbl {
        parent: IUnknownVtbl {
            query_interface: source_query_interface,
            add_ref: source_add_ref,
            release: source_release,
        },
        query_continue_drag: source_query_continue_drag,
        give_feedback: source_give_feedback,
    };

    fn supports_hdrop(format: &FORMATETC) -> bool {
        format.cfFormat == CF_HDROP
            && format.dwAspect == DVASPECT_CONTENT
            && format.lindex == -1
            && (format.tymed & (TYMED_HGLOBAL as u32)) != 0
    }

    fn guid_eq(a: &GUID, b: &GUID) -> bool {
        a.data1 == b.data1 && a.data2 == b.data2 && a.data3 == b.data3 && a.data4 == b.data4
    }

    fn build_hdrop_blob(files: &[PathBuf]) -> Option<windows_sys::Win32::Foundation::HGLOBAL> {
        if files.is_empty() {
            return None;
        }

        let mut total_bytes = size_of::<DROPFILES>();
        let mut encoded: Vec<Vec<u16>> = Vec::with_capacity(files.len());

        for path in files {
            let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
            wide.push(0);
            total_bytes = total_bytes.checked_add(wide.len().checked_mul(2)?)?;
            encoded.push(wide);
        }

        total_bytes = total_bytes.checked_add(2)?;
        let hglobal = unsafe { GlobalAlloc(GHND, total_bytes) };
        if hglobal.is_null() {
            return None;
        }

        unsafe {
            let ptr = GlobalLock(hglobal) as *mut u8;
            if ptr.is_null() {
                let _ = GlobalFree(hglobal);
                return None;
            }

            let header = DROPFILES {
                pFiles: size_of::<DROPFILES>() as u32,
                pt: std::mem::zeroed(),
                fNC: 0,
                fWide: 1,
            };
            (ptr as *mut DROPFILES).write(header);

            let mut cursor = ptr.add(size_of::<DROPFILES>()) as *mut u16;
            for wide in &encoded {
                std::ptr::copy_nonoverlapping(wide.as_ptr(), cursor, wide.len());
                cursor = cursor.add(wide.len());
            }
            *cursor = 0;

            GlobalUnlock(hglobal);
        }

        Some(hglobal)
    }

    pub fn init_ole() {
        unsafe {
            let _ = OleInitialize(null_mut());
        }
    }

    pub fn uninit_ole() {
        unsafe {
            OleUninitialize();
        }
    }

    pub fn begin_file_drag(path: PathBuf) -> bool {
        begin_file_drag_paths(vec![path])
    }

    fn begin_file_drag_paths(paths: Vec<PathBuf>) -> bool {
        if paths.is_empty() {
            return false;
        }

        unsafe {
            let data = FileDataObject::new(paths);
            let source = DropSource::new();
            let data_ptr = Box::into_raw(data);
            let source_ptr = Box::into_raw(source);
            let mut effect = DROPEFFECT_NONE;
            let hr = DoDragDrop(
                data_ptr as *mut c_void,
                source_ptr as *mut c_void,
                DROPEFFECT_COPY,
                &mut effect,
            );

            let _ = data_release(data_ptr as *mut c_void);
            let _ = source_release(source_ptr as *mut c_void);

            hr >= 0
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use std::path::PathBuf;

    #[allow(dead_code)]
    pub fn init_ole() {}
    #[allow(dead_code)]
    pub fn uninit_ole() {}
    #[allow(dead_code)]
    pub fn begin_file_drag(_path: PathBuf) -> bool {
        false
    }
}

pub use imp::*;
