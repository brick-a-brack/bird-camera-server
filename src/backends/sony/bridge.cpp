/*
 * Flat C bridge over the Sony Camera Remote SDK (CrSDK). See bridge.h.
 *
 * One SonyCamera wraps a single session and implements IDeviceCallback so it can
 * block sn_connect() until OnConnected and sn_capture() until OnCompleteDownload.
 */
#include "bridge.h"

#include "CRSDK/CameraRemote_SDK.h"
#include "CRSDK/ICrCameraObjectInfo.h"
#include "CRSDK/IDeviceCallback.h"
#include "CRSDK/CrDeviceProperty.h"
#include "CRSDK/CrCommandData.h"
#include "CRSDK/CrImageDataBlock.h"

#include <atomic>
#include <chrono>
#include <condition_variable>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <mutex>
#include <string>
#include <thread>
#include <vector>

#ifdef _WIN32
#include <windows.h>
#endif

namespace SDK = SCRSDK;

// The Cr integer/char typedefs (CrChar, CrInt8u, CrInt32, …) live at GLOBAL
// scope (CrTypes.h has no namespace); only CrDeviceHandle, CrError, the enums and
// the classes are in SCRSDK. CrChar is wchar_t on Windows (the DLL is built
// UNICODE) and char elsewhere.
using tchar   = CrChar;
using tstring = std::basic_string<CrChar>;

// ---------------------------------------------------------------------------
// String / path helpers
// ---------------------------------------------------------------------------

static std::string to_utf8(const tchar* s) {
    if (!s) return std::string();
#ifdef _WIN32
    int len = WideCharToMultiByte(CP_UTF8, 0, s, -1, nullptr, 0, nullptr, nullptr);
    if (len <= 0) return std::string();
    std::string out(static_cast<size_t>(len - 1), '\0');
    WideCharToMultiByte(CP_UTF8, 0, s, -1, out.data(), len, nullptr, nullptr);
    return out;
#else
    return std::string(s);
#endif
}

static void copy_cstr(char* dst, size_t cap, const std::string& src) {
    if (cap == 0) return;
    size_t n = src.size() < cap - 1 ? src.size() : cap - 1;
    std::memcpy(dst, src.data(), n);
    dst[n] = '\0';
}

static std::string hex_encode(const uint8_t* data, size_t len) {
    static const char* H = "0123456789abcdef";
    std::string out;
    out.reserve(len * 2);
    for (size_t i = 0; i < len; ++i) {
        out.push_back(H[data[i] >> 4]);
        out.push_back(H[data[i] & 0x0F]);
    }
    return out;
}

// Directory where the SDK drops captured stills; read back then deleted.
static tstring g_tempdir;

static void ensure_tempdir() {
    if (!g_tempdir.empty()) return;
#ifdef _WIN32
    wchar_t buf[MAX_PATH];
    DWORD n = GetTempPathW(MAX_PATH, buf);
    tstring dir(buf, n);
    dir += L"toucan_sony\\";
    CreateDirectoryW(dir.c_str(), nullptr);
    g_tempdir = dir;
#else
    const char* base = std::getenv("TMPDIR");
    std::string dir = base && *base ? std::string(base) : std::string("/tmp");
    if (dir.back() != '/') dir += '/';
    dir += "toucan_sony/";
    ::system((std::string("mkdir -p '") + dir + "'").c_str());
    g_tempdir = dir;
#endif
}

// Reads a whole file into a malloc'd buffer. Returns true on success.
static bool read_whole_file(const tstring& path, uint8_t** out, uint32_t* size) {
#ifdef _WIN32
    FILE* f = _wfopen(path.c_str(), L"rb");
#else
    FILE* f = std::fopen(path.c_str(), "rb");
#endif
    if (!f) return false;
    std::fseek(f, 0, SEEK_END);
    long len = std::ftell(f);
    std::fseek(f, 0, SEEK_SET);
    if (len <= 0) { std::fclose(f); return false; }
    uint8_t* buf = static_cast<uint8_t*>(std::malloc(static_cast<size_t>(len)));
    if (!buf) { std::fclose(f); return false; }
    size_t rd = std::fread(buf, 1, static_cast<size_t>(len), f);
    std::fclose(f);
    if (rd != static_cast<size_t>(len)) { std::free(buf); return false; }
    *out = buf;
    *size = static_cast<uint32_t>(len);
    return true;
}

static void delete_file(const tstring& path) {
#ifdef _WIN32
    _wremove(path.c_str());
#else
    std::remove(path.c_str());
#endif
}

// Byte width of one element for a CrDataType (0 for string / unsupported).
static uint32_t element_width(uint32_t value_type) {
    switch (value_type & 0x000F) {
        case 0x0001: return 1; // UInt8 / Int8
        case 0x0002: return 2; // UInt16 / Int16
        case 0x0003: return 4; // UInt32 / Int32
        case 0x0004: return 8; // UInt64 / Int64
        default:     return 0;
    }
}

// ---------------------------------------------------------------------------
// SonyCamera: one session + its IDeviceCallback
// ---------------------------------------------------------------------------

class SonyCamera : public SDK::IDeviceCallback {
public:
    // IDeviceCallback's destructor is not virtual; sessions are always deleted
    // through a SonyCamera*, so declare one here to make the deletion well-defined.
    virtual ~SonyCamera() = default;

    SDK::CrDeviceHandle handle = 0;
    SDK::ICrEnumCameraObjectInfo* enum_list = nullptr; // kept alive so `info` stays valid

    std::mutex mtx;
    std::condition_variable cv;
    bool connected     = false;
    bool conn_failed   = false;
    bool download_done = false;
    tstring download_path;

    // The SDK can drop the device at any time (OnDisconnected). Every entry point
    // checks this: once the body is gone its handle is dead, and the SDK answers
    // CrError_Api_InvalidCalled to everything — which read as "not ready yet" and
    // had live view polling a corpse instead of reporting the device as lost.
    std::atomic<bool> gone{false};

    // The device is only usable once the SDK has finished its initial property
    // sync, which it signals with the first OnPropertyChanged. Writing properties
    // before that races the adaptor's own setup.
    bool props_ready = false;

    // Live view polls at ~30 Hz; only the first SDK refusal of a session is logged.
    std::atomic<bool> lv_logged{false};

    // Last OnWarning code. A refused shutter is reported here (and only here):
    // the release command itself still returns CrError_None.
    std::atomic<unsigned> last_warning{0};

    // Last OnError code, for the same reason: a connect that never completes only
    // explains itself through this callback.
    std::atomic<unsigned> last_error{0};

    void log_lv_once(const char* what, SDK::CrError err) {
        if (lv_logged.exchange(true)) return;
        std::fprintf(stderr, "[sony] live view refused by the SDK: %s -> 0x%08X\n",
                     what, static_cast<unsigned>(err));
    }

    // --- IDeviceCallback ---
    void OnConnected(SDK::DeviceConnectionVersioin) override {
        std::lock_guard<std::mutex> lk(mtx);
        connected = true;
        cv.notify_all();
    }
    void OnDisconnected(CrInt32u error) override {
        // error == 0 is the callback for our own sn_disconnect — routine, not news.
        if (error != 0) {
            std::fprintf(stderr, "[sony] device dropped by the SDK -> 0x%08X\n",
                         static_cast<unsigned>(error));
        }
        gone.store(true);
        std::lock_guard<std::mutex> lk(mtx);
        connected = false;
        cv.notify_all();
    }
    void OnError(CrInt32u error) override {
        last_error.store(static_cast<unsigned>(error));
        std::lock_guard<std::mutex> lk(mtx);
        if (!connected) conn_failed = true; // surface connect-time failures
        cv.notify_all();
    }
    void OnCompleteDownload(tchar* filename, CrInt32u) override {
        std::lock_guard<std::mutex> lk(mtx);
        download_path = filename ? tstring(filename) : tstring();
        download_done = true;
        cv.notify_all();
    }
    void OnNotifyPostViewImage(tchar*, CrInt32u) override {}
    // Warnings are routine (every capture emits CautionDisplay when no card is in,
    // and CrNotify_Captured_Event on exposure). Keep the last one instead of
    // logging each: it is what explains a shutter that never fired.
    void OnWarning(CrInt32u warning) override {
        last_warning.store(static_cast<unsigned>(warning));
    }
    void OnPropertyChanged() override {
        std::lock_guard<std::mutex> lk(mtx);
        props_ready = true;
        cv.notify_all();
    }
    void OnLvPropertyChanged() override {}
};

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

static std::atomic<bool> g_inited{false};

// The most recent camera enumeration, kept alive so sn_connect can reuse it instead
// of paying a second ~3 s EnumCameraObjects scan. Only ever touched on the single
// actor thread (sn_list_devices / sn_connect / sn_disconnect are all serialized
// there by the Rust actor), so no locking is needed. It is FROZEN — never released
// or replaced — while any session is open (g_open_sessions > 0); that guarantees an
// ICrCameraObjectInfo handed to a live session stays valid for the session's whole
// lifetime, since the only place that would free the list won't while it is in use.
static SDK::ICrEnumCameraObjectInfo* g_enum_list = nullptr;
static int g_open_sessions = 0;

// Finds the camera whose USB id matches native_id inside an enumeration list.
static const SDK::ICrCameraObjectInfo* find_in_list(
        SDK::ICrEnumCameraObjectInfo* list, const char* native_id) {
    if (!list) return nullptr;
    int total = static_cast<int>(list->GetCount());
    for (int i = 0; i < total; ++i) {
        const SDK::ICrCameraObjectInfo* ci = list->GetCameraObjectInfo(i);
        if (!ci) continue;
        if (hex_encode(ci->GetId(), ci->GetIdSize()) == native_id) return ci;
    }
    return nullptr;
}

// Reads one property's current raw value; returns false if unavailable.
static bool get_current_value(SDK::CrDeviceHandle handle, uint32_t code, uint64_t* out) {
    CrInt32u codes[1] = { code };
    SDK::CrDeviceProperty* props = nullptr;
    CrInt32 num = 0;
    if (SDK::GetSelectDeviceProperties(handle, 1, codes, &props, &num) != SDK::CrError_None
        || num < 1 || !props) {
        return false;
    }
    *out = props[0].GetCurrentValue();
    SDK::ReleaseDeviceProperties(handle, props);
    return true;
}

// Sets one device property, resolving its value type from the camera so callers
// only need the property code + raw value.
//
// SetDeviceProperty is asynchronous: the body applies the change a moment later
// and signals it via OnPropertyChanged. We block until the value actually takes
// (or the body snaps it to a nearby valid value) so the caller's follow-up read
// reflects the new state instead of the stale one — otherwise the UI glitches.
// Bounded, so a value the body silently rejects can't hang the request.
static SDK::CrError set_property(SDK::CrDeviceHandle handle, uint32_t code, uint64_t value) {
    CrInt32u codes[1] = { code };
    SDK::CrDeviceProperty* props = nullptr;
    CrInt32 num = 0;
    if (SDK::GetSelectDeviceProperties(handle, 1, codes, &props, &num) != SDK::CrError_None
        || num < 1 || !props) {
        return SDK::CrError_Generic;
    }
    SDK::CrDataType value_type = props[0].GetValueType();
    uint64_t old_value = props[0].GetCurrentValue();
    SDK::ReleaseDeviceProperties(handle, props);

    SDK::CrDeviceProperty prop;
    prop.SetCode(code);
    prop.SetValueType(value_type);
    prop.SetCurrentValue(value);
    SDK::CrError err = SDK::SetDeviceProperty(handle, &prop);
    if (err != SDK::CrError_None) {
        return err;
    }

    for (int i = 0; i < 40; ++i) { // up to ~2 s
        std::this_thread::sleep_for(std::chrono::milliseconds(50));
        uint64_t now = old_value;
        if (get_current_value(handle, code, &now) && (now == value || now != old_value)) {
            break;
        }
    }
    return SDK::CrError_None;
}

// ---------------------------------------------------------------------------
// C API
// ---------------------------------------------------------------------------

extern "C" {

int sn_init(void) {
    if (g_inited.load()) return SN_OK;
    bool ok = SDK::Init(0);
    std::fprintf(stderr, "[sony-bridge] SDK::Init -> %s (SDK version 0x%08X)\n",
                 ok ? "OK" : "FAIL", SDK::GetSDKVersion());
    if (!ok) return SN_ERR;
    ensure_tempdir();
    g_inited.store(true);
    return SN_OK;
}

void sn_release(void) {
    if (!g_inited.load()) return;
    // Free the cached enumeration before tearing the SDK down (its objects belong to
    // the SDK). Sessions are already cleared by the time we get here.
    if (g_enum_list) { g_enum_list->Release(); g_enum_list = nullptr; }
    g_open_sessions = 0;
    SDK::Release();
    g_inited.store(false);
}

int sn_list_devices(SnDeviceInfo* out, int capacity, int time_in_sec) {
    if (sn_init() != SN_OK) return SN_ERR;

    // `time_in_sec` is a floor the SDK waits before returning; the real USB scan is
    // longer (measured ~6.5 s at 3, ~2.2 s at 1 on an α7 IV). The Rust side asks for
    // a thorough scan (3) only when nothing is cached yet — needed to discover a cold
    // / freshly plugged body — and a fast scan (1) to refresh an already-known one,
    // so steady-state re-detection isn't stuck behind a 6.5 s enumeration. This never
    // runs inline for /cameras (that serves the cache), so the scan can't stall a
    // route; it can still block the actor thread, hence keeping it short when it can.
    if (time_in_sec < 1) time_in_sec = 1;
    SDK::ICrEnumCameraObjectInfo* list = nullptr;
    if (SDK::EnumCameraObjects(&list, static_cast<CrInt8u>(time_in_sec)) != SDK::CrError_None
        || !list) {
        return 0; // no cameras (or enumeration failed) — treat as empty
    }

    int total = static_cast<int>(list->GetCount());
    int n = total < capacity ? total : capacity;
    for (int i = 0; i < n; ++i) {
        const SDK::ICrCameraObjectInfo* ci = list->GetCameraObjectInfo(i);
        if (!ci) { n = i; break; }
        copy_cstr(out[i].model, SN_MAX_MODEL, to_utf8(ci->GetModel()));
        copy_cstr(out[i].id, SN_MAX_ID, hex_encode(ci->GetId(), ci->GetIdSize()));
        copy_cstr(out[i].conn_type, SN_MAX_CONN, to_utf8(ci->GetConnectionTypeName()));
    }
    // Keep this enumeration alive for sn_connect to reuse (saves it a second ~3 s
    // scan), but only when no session is open — a list a live session may be
    // referencing must never be freed under it. While sessions are open we've still
    // filled `out` above for the cache; just drop this scan's list.
    if (g_open_sessions == 0) {
        if (g_enum_list) g_enum_list->Release();
        g_enum_list = list;
    } else {
        list->Release();
    }
    return n;
}

void* sn_connect(const char* native_id, uint32_t* err) {
    auto fail = [&](uint32_t code) -> void* {
        if (err) *err = code;
        return nullptr;
    };

    if (sn_init() != SN_OK) return fail(SN_CONNECT_NOT_FOUND);

    // Reuse the enumeration the background cache already did: the scan that made this
    // device appear in /cameras left its list in g_enum_list, so connecting need not
    // pay another ~3 s EnumCameraObjects. The list is frozen while sessions are open,
    // and this whole function runs on the actor thread so no refresh can free it
    // mid-connect — a match found here stays valid for the session, we don't own it.
    // `owned_list` stays null on this fast path.
    const SDK::ICrCameraObjectInfo* match = find_in_list(g_enum_list, native_id);
    SDK::ICrEnumCameraObjectInfo* owned_list = nullptr;

    // Fallback: nothing cached yet, or the device was plugged in after the cache
    // froze — enumerate now and own that list for the session's lifetime.
    if (!match) {
        SDK::CrError enum_err = SDK::EnumCameraObjects(&owned_list);
        if (enum_err != SDK::CrError_None || !owned_list) {
            return fail(static_cast<uint32_t>(enum_err));
        }
        match = find_in_list(owned_list, native_id);
        if (!match) {
            owned_list->Release();
            return fail(SN_CONNECT_NOT_FOUND);
        }
    }

    SonyCamera* cam = new SonyCamera();
    cam->enum_list = owned_list; // null when reusing the shared g_enum_list

    // Connect defaults to CrSdkControlMode_Remote + reconnect ON. The call returns
    // immediately; the session is ready only once OnConnected fires.
    SDK::CrError conn_err = SDK::Connect(
        const_cast<SDK::ICrCameraObjectInfo*>(match), cam, &cam->handle);
    if (conn_err != SDK::CrError_None) {
        if (cam->enum_list) cam->enum_list->Release();
        delete cam;
        return fail(static_cast<uint32_t>(conn_err));
    }

    {
        std::unique_lock<std::mutex> lk(cam->mtx);
        cam->cv.wait_for(lk, std::chrono::seconds(15),
                         [&] { return cam->connected || cam->conn_failed; });
        if (!cam->connected) {
            // The body is on the bus but never completed the handshake. It reports
            // why through OnError (CrError_Connect_TimeOut 0x8208 when it still
            // believes an earlier PC Remote session is open).
            uint32_t why = cam->last_error.load();
            lk.unlock();
            SDK::ReleaseDevice(cam->handle);
            if (cam->enum_list) cam->enum_list->Release();
            delete cam;
            return fail(why ? why : static_cast<uint32_t>(SDK::CrError_Connect_TimeOut));
        }
    }

    // OnConnected only means the transport is up: the SDK is still pulling the
    // device's property set behind our back, and it signals the end of that sync
    // with the first OnPropertyChanged. Writing properties before then races the
    // adaptor's own setup, which is what killed the session (OnError /
    // OnDisconnected 0x8702 = CrError_Adaptor_GetInfo) on the very first write.
    {
        std::unique_lock<std::mutex> lk(cam->mtx);
        cam->cv.wait_for(lk, std::chrono::seconds(5),
                         [&] { return cam->props_ready || !cam->connected; });
    }
    if (cam->gone.load()) {
        uint32_t why = cam->last_error.load();
        SDK::ReleaseDevice(cam->handle);
        if (cam->enum_list) cam->enum_list->Release();
        delete cam;
        return fail(why ? why : static_cast<uint32_t>(SDK::CrError_Connect_TimeOut));
    }

    // Route stills to the PC only. HostPC+MemoryCard makes the body refuse to fire
    // when no card is inserted (it raises CrWarning_CautionDisplay and downloads
    // nothing), and we hand the JPEG straight back over HTTP anyway — the card copy
    // was never part of what the API promises. Each setup step is reported: a body
    // that refuses one of them is worth knowing about, none is fatal on its own.
    SDK::CrError dest_err = set_property(cam->handle,
                                         SDK::CrDeviceProperty_StillImageStoreDestination,
                                         SDK::CrStillImageStoreDestination_HostPC);
    tstring prefix; // empty → SDK default
    SDK::CrError save_err = SDK::SetSaveInfo(cam->handle, const_cast<tchar*>(g_tempdir.c_str()),
                                             const_cast<tchar*>(prefix.c_str()),
                                             SDK::CrSETSAVEINFO_AUTO_NUMBER);
    SDK::CrError lv_set = SDK::SetDeviceSetting(cam->handle, SDK::Setting_Key_EnableLiveView,
                                                SDK::CrDeviceSetting_Enable);
    if (dest_err != SDK::CrError_None || save_err != SDK::CrError_None
        || lv_set != SDK::CrError_None) {
        std::fprintf(stderr,
                     "[sony] setup refused by the body: StillImageStoreDestination->0x%08X "
                     "SetSaveInfo->0x%08X EnableLiveView->0x%08X\n",
                     static_cast<unsigned>(dest_err), static_cast<unsigned>(save_err),
                     static_cast<unsigned>(lv_set));
    }

    // Session is live: freeze g_enum_list until it (and any sibling) closes, so the
    // enumeration this session may be referencing is never freed under it.
    g_open_sessions++;
    return cam;
}

int sn_is_alive(void* handle) {
    SonyCamera* cam = static_cast<SonyCamera*>(handle);
    if (!cam) return 0;
    return cam->gone.load() ? 0 : 1;
}

void sn_disconnect(void* handle) {
    SonyCamera* cam = static_cast<SonyCamera*>(handle);
    if (!cam) return;

    // SDK::Disconnect is asynchronous: the body only finishes tearing the PC Remote
    // session down once it calls OnDisconnected. Releasing the device before that
    // leaves the camera believing the session is still open, so the *next* Connect
    // is refused with CrError_Connect_SessionAlreadyOpened (0x8210) or
    // CrError_Connect_TimeOut (0x8208) until the body is power-cycled. Wait for the
    // callback (bounded, so a body that never answers can't hang the shutdown path),
    // then release. If the device was already dropped (gone), connected is already
    // false and this returns at once.
    SDK::Disconnect(cam->handle);
    {
        std::unique_lock<std::mutex> lk(cam->mtx);
        cam->cv.wait_for(lk, std::chrono::seconds(5), [&] { return !cam->connected; });
    }
    SDK::ReleaseDevice(cam->handle);
    if (cam->enum_list) cam->enum_list->Release();
    delete cam;

    // One fewer session referencing g_enum_list; when it reaches zero the next cache
    // refresh is free to replace the frozen enumeration again.
    if (g_open_sessions > 0) g_open_sessions--;
}

int sn_get_parameters(void* handle, SnParam* out, int capacity) {
    SonyCamera* cam = static_cast<SonyCamera*>(handle);
    if (!cam) return SN_ERR;

    SDK::CrDeviceProperty* props = nullptr;
    CrInt32 num = 0;
    if (SDK::GetDeviceProperties(cam->handle, &props, &num) != SDK::CrError_None || !props) {
        return SN_ERR;
    }

    int n = static_cast<int>(num) < capacity ? static_cast<int>(num) : capacity;
    for (int i = 0; i < n; ++i) {
        SDK::CrDeviceProperty& p = props[i];
        SnParam& o = out[i];
        o.code       = p.GetCode();
        o.current    = p.GetCurrentValue();
        o.writable   = p.IsSetEnableCurrentValue() ? 1 : 0;
        o.value_type = p.GetValueType();
        o.num_options = 0;

        uint32_t w = element_width(o.value_type);
        const uint8_t* vals = p.GetValues();
        uint32_t vsize = p.GetValueSize();
        if (w != 0 && vals && vsize >= w) {
            int count = static_cast<int>(vsize / w);
            if (count > SN_MAX_OPTIONS) count = SN_MAX_OPTIONS;
            for (int k = 0; k < count; ++k) {
                uint64_t v = 0;
                std::memcpy(&v, vals + static_cast<size_t>(k) * w, w);
                o.options[k] = static_cast<int64_t>(v);
            }
            o.num_options = count;
        }
    }
    SDK::ReleaseDeviceProperties(cam->handle, props);
    return n;
}

int sn_set_parameter(void* handle, uint32_t code, uint64_t value) {
    SonyCamera* cam = static_cast<SonyCamera*>(handle);
    if (!cam) return SN_ERR;
    return set_property(cam->handle, code, value);
}

int sn_set_iso_auto(void* handle, int enable) {
    SonyCamera* cam = static_cast<SonyCamera*>(handle);
    if (!cam) return SN_ERR;

    const uint32_t ISO_AUTO = 0x00FFFFFF; // bits 0-23 all set = AUTO
    if (enable) {
        return set_property(cam->handle, SDK::CrDeviceProperty_IsoSensitivity, ISO_AUTO);
    }

    // Leaving auto: the ISO property has no separate toggle, so pick the lowest
    // concrete value the body currently offers.
    CrInt32u code = SDK::CrDeviceProperty_IsoSensitivity;
    SDK::CrDeviceProperty* props = nullptr;
    CrInt32 num = 0;
    if (SDK::GetSelectDeviceProperties(cam->handle, 1, &code, &props, &num) != SDK::CrError_None
        || num < 1 || !props) {
        return SDK::CrError_Generic;
    }
    uint32_t w = element_width(props[0].GetValueType());
    const uint8_t* vals = props[0].GetValues();
    uint32_t vsize = props[0].GetValueSize();
    bool found = false;
    uint32_t lowest = 0;
    if (w != 0 && vals) {
        for (uint32_t off = 0; off + w <= vsize; off += w) {
            uint64_t v = 0;
            std::memcpy(&v, vals + off, w);
            uint32_t iso = static_cast<uint32_t>(v) & 0x00FFFFFF;
            if (iso == 0x00FFFFFF) continue; // skip AUTO
            if (!found || iso < (lowest & 0x00FFFFFF)) {
                lowest = static_cast<uint32_t>(v);
                found = true;
            }
        }
    }
    SDK::ReleaseDeviceProperties(cam->handle, props);
    if (!found) return SDK::CrError_Generic;
    return set_property(cam->handle, code, lowest);
}

int sn_get_live_view(void* handle, uint8_t** out, uint32_t* size) {
    SonyCamera* cam = static_cast<SonyCamera*>(handle);
    if (!cam) return SN_ERR;
    // A dropped device is not "not ready": report it so the stream ends instead of
    // polling a dead handle for ever.
    if (cam->gone.load()) return SN_ERR;

    // Every failure below collapses to SN_NOT_READY (the route polls and retries),
    // so log the first reason per session: without it a live view that never starts
    // is indistinguishable from one that is merely warming up.
    SDK::CrImageInfo info;
    SDK::CrError err = SDK::GetLiveViewImageInfo(cam->handle, &info);
    if (err != SDK::CrError_None) {
        cam->log_lv_once("GetLiveViewImageInfo", err);
        return SN_NOT_READY;
    }
    // An empty buffer is the stream warming up, not a failure — stay quiet.
    uint32_t buf_size = info.GetBufferSize();
    if (buf_size < 1) return SN_NOT_READY;

    std::vector<CrInt8u> buffer(buf_size);
    SDK::CrImageDataBlock img;
    img.SetSize(buf_size);
    img.SetData(buffer.data());
    err = SDK::GetLiveViewImage(cam->handle, &img);
    if (err != SDK::CrError_None) {
        cam->log_lv_once("GetLiveViewImage", err);
        return SN_NOT_READY;
    }
    uint32_t img_size = img.GetImageSize();
    if (img_size == 0) return SN_NOT_READY;

    uint8_t* result = static_cast<uint8_t*>(std::malloc(img_size));
    if (!result) return SN_ERR;
    std::memcpy(result, img.GetImageData(), img_size);
    *out = result;
    *size = img_size;
    return SN_OK;
}

int sn_capture(void* handle, uint8_t** out, uint32_t* size) {
    SonyCamera* cam = static_cast<SonyCamera*>(handle);
    if (!cam) return SN_ERR;
    if (cam->gone.load()) return SN_ERR;

    {
        std::lock_guard<std::mutex> lk(cam->mtx);
        cam->download_done = false;
        cam->download_path.clear();
    }
    cam->last_warning.store(0);

    // Use S1andRelease (autofocus + shutter), not a bare Release (shutter only): in
    // any AF focus mode the body refuses to fire until focus is locked, so a plain
    // full-press produces no image and we'd just time out. S1andRelease makes the
    // camera focus first, then fire.
    SDK::SendCommand(cam->handle, SDK::CrCommandId_S1andRelease, SDK::CrCommandParam_Down);

    // Hold the press until the shot is actually taken, instead of a blind 35 ms:
    // poll FocusIndication so we neither cut autofocus short (releasing the button
    // mid-AF cancels the shot) nor hang forever on a body that can't focus. Manual
    // focus never reports a lock, so a body in MF just fires and OnCompleteDownload
    // ends the wait early.
    auto is_downloaded = [&] {
        std::lock_guard<std::mutex> lk(cam->mtx);
        return cam->download_done;
    };
    bool af_failed = false;
    for (int i = 0; i < 80; ++i) { // up to ~4 s of AF
        if (is_downloaded()) break; // already fired (incl. MF / release-priority)
        uint64_t fi = 0;
        if (get_current_value(cam->handle, SDK::CrDeviceProperty_FocusIndication, &fi)) {
            uint32_t v = static_cast<uint32_t>(fi);
            if (v == SDK::CrFocusIndicator_Focused_AF_S
                || v == SDK::CrFocusIndicator_Focused_AF_C
                || v == SDK::CrFocusIndicator_TrackingSubject_AF_C) {
                break; // locked — the shot fires now
            }
            if (v == SDK::CrFocusIndicator_NotFocused_AF_S
                || v == SDK::CrFocusIndicator_NotFocused_AF_C) {
                af_failed = true;
                break;
            }
        }
        std::this_thread::sleep_for(std::chrono::milliseconds(50));
    }

    SDK::SendCommand(cam->handle, SDK::CrCommandId_S1andRelease, SDK::CrCommandParam_Up);

    // Autofocus explicitly failed and nothing was shot: don't make the caller wait
    // out the full download timeout — cancel the pending shooting and say so.
    if (af_failed && !is_downloaded()) {
        SDK::SendCommand(cam->handle, SDK::CrCommandId_CancelShooting, SDK::CrCommandParam_Down);
        SDK::SendCommand(cam->handle, SDK::CrCommandId_CancelShooting, SDK::CrCommandParam_Up);
        std::fprintf(stderr, "[sony] capture: autofocus could not lock — no shot\n");
        return SN_ERR_AF;
    }

    tstring path;
    {
        std::unique_lock<std::mutex> lk(cam->mtx);
        if (!cam->cv.wait_for(lk, std::chrono::seconds(20),
                              [&] { return cam->download_done; })) {
            // The shutter never fired, or the body kept the file. The camera only
            // ever says why through OnWarning (0x0002008C = CrWarning_CautionDisplay
            // — read the caution shown on the body: no card, AF failed, …).
            std::fprintf(stderr,
                         "[sony] capture: no download within 20 s "
                         "(last warning 0x%08X)\n",
                         cam->last_warning.load());
            return SN_ERR;
        }
        path = cam->download_path;
    }
    if (path.empty()) {
        std::fprintf(stderr, "[sony] capture: download reported with an empty path\n");
        return SN_ERR;
    }

    bool ok = read_whole_file(path, out, size);
    delete_file(path);
    return ok ? SN_OK : SN_ERR;
}

void sn_free(uint8_t* p) {
    std::free(p);
}

} // extern "C"
