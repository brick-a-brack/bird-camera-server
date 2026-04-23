@import AVFoundation;
@import ImageIO;
@import Foundation;
@import CoreMediaIO;

#include "bridge.h"
#include <stdlib.h>
#include <string.h>
#include <math.h>

#pragma clang diagnostic ignored "-Wdeprecated-declarations"

// ---------------------------------------------------------------------------
// WcFrameDelegate — stores the latest raw pixel buffer from the capture queue.
// ---------------------------------------------------------------------------

@interface WcFrameDelegate : NSObject <AVCaptureVideoDataOutputSampleBufferDelegate>
- (nullable NSData *)encodeLatestFrameAsJPEG;
@end

@implementation WcFrameDelegate {
    NSLock              *_lock;
    CVPixelBufferRef     _latestBuffer;
    dispatch_semaphore_t _firstFrameSem;
    BOOL                 _hasFrame;
}

- (instancetype)init {
    if ((self = [super init])) {
        _lock          = [[NSLock alloc] init];
        _latestBuffer  = NULL;
        _firstFrameSem = dispatch_semaphore_create(0);
        _hasFrame      = NO;
    }
    return self;
}

- (void)dealloc {
    [_lock lock];
    if (_latestBuffer) { CVPixelBufferRelease(_latestBuffer); _latestBuffer = NULL; }
    [_lock unlock];
}

- (void)captureOutput:(AVCaptureOutput *)output
didOutputSampleBuffer:(CMSampleBufferRef)sampleBuffer
       fromConnection:(AVCaptureConnection *)connection {
    CVPixelBufferRef pb = CMSampleBufferGetImageBuffer(sampleBuffer);
    if (!pb) return;
    CVPixelBufferRetain(pb);
    [_lock lock];
    if (_latestBuffer) CVPixelBufferRelease(_latestBuffer);
    _latestBuffer = pb;
    if (!_hasFrame) { _hasFrame = YES; dispatch_semaphore_signal(_firstFrameSem); }
    [_lock unlock];
}

- (nullable NSData *)encodeLatestFrameAsJPEG {
    if (!_hasFrame) {
        dispatch_semaphore_wait(_firstFrameSem,
            dispatch_time(DISPATCH_TIME_NOW, 2LL * NSEC_PER_SEC));
    }
    [_lock lock];
    CVPixelBufferRef pb = _latestBuffer ? CVPixelBufferRetain(_latestBuffer) : NULL;
    [_lock unlock];
    if (!pb) return nil;

    CVPixelBufferLockBaseAddress(pb, kCVPixelBufferLock_ReadOnly);
    size_t width       = CVPixelBufferGetWidth(pb);
    size_t height      = CVPixelBufferGetHeight(pb);
    size_t bytesPerRow = CVPixelBufferGetBytesPerRow(pb);
    void  *baseAddr    = CVPixelBufferGetBaseAddress(pb);

    CGColorSpaceRef cs = CGColorSpaceCreateDeviceRGB();
    CGContextRef ctx = CGBitmapContextCreate(baseAddr, width, height, 8, bytesPerRow, cs,
        kCGBitmapByteOrder32Little | kCGImageAlphaPremultipliedFirst);
    CGImageRef img = CGBitmapContextCreateImage(ctx);
    CGContextRelease(ctx);
    CGColorSpaceRelease(cs);
    CVPixelBufferUnlockBaseAddress(pb, kCVPixelBufferLock_ReadOnly);
    CVPixelBufferRelease(pb);
    if (!img) return nil;

    NSMutableData *jpeg = [NSMutableData data];
    CGImageDestinationRef dest = CGImageDestinationCreateWithData(
        (__bridge CFMutableDataRef)jpeg, CFSTR("public.jpeg"), 1, NULL);
    if (!dest) { CGImageRelease(img); return nil; }
    CGImageDestinationAddImage(dest, img, (__bridge CFDictionaryRef)@{
        (__bridge id)kCGImageDestinationLossyCompressionQuality: @(0.75)
    });
    CGImageDestinationFinalize(dest);
    CFRelease(dest);
    CGImageRelease(img);
    return jpeg.length > 0 ? jpeg : nil;
}
@end

// ---------------------------------------------------------------------------
// WcSessionHandle
// ---------------------------------------------------------------------------

@interface WcSessionHandle : NSObject
@property (nonatomic, strong) AVCaptureSession *session;
@property (nonatomic, strong) AVCaptureDevice  *device;
@property (nonatomic, strong) WcFrameDelegate  *delegate;
@property (nonatomic, strong) dispatch_queue_t  captureQueue;
- (void)setCmioDeviceID:(uint32_t)devID;
- (uint32_t)cmioDeviceID;
@end

@implementation WcSessionHandle {
    uint32_t _cmioDeviceID; // CMIOObjectID; 0 = not found
}
- (void)setCmioDeviceID:(uint32_t)devID { _cmioDeviceID = devID; }
- (uint32_t)cmioDeviceID { return _cmioDeviceID; }
@end

// ---------------------------------------------------------------------------
// CMIOHardware helpers
// ---------------------------------------------------------------------------

// Mapping from CMIO control class ID to our parameter kind string.
typedef struct { uint32_t classID; const char *kind; } CmioKindEntry;

static const CmioKindEntry kCmioKinds[] = {
    { kCMIOBrightnessControlClassID,            "brightness"                },
    { kCMIOContrastControlClassID,              "contrast"                  },
    { kCMIOGainControlClassID,                  "gain"                      },
    { kCMIOSaturationControlClassID,            "saturation"                },
    { kCMIOSharpnessControlClassID,             "sharpness"                 },
    { kCMIOHueControlClassID,                   "hue"                       },
    { kCMIOTemperatureControlClassID,           "white_balance_temperature"  },
    { kCMIOBacklightCompensationControlClassID, "backlight_compensation"     },
    { kCMIOExposureControlClassID,              "exposure_time_absolute"     },
    { kCMIOFocusControlClassID,                 "focus_absolute"             },
    { kCMIOZoomControlClassID,                  "zoom_absolute"              },
    { kCMIOPanControlClassID,                   "pan_absolute"               },
    { kCMIOTiltControlClassID,                  "tilt_absolute"              },
};
static const int kCmioKindCount = (int)(sizeof(kCmioKinds) / sizeof(kCmioKinds[0]));

static const char *cmio_kind_for_class(uint32_t classID) {
    for (int i = 0; i < kCmioKindCount; i++)
        if (kCmioKinds[i].classID == classID) return kCmioKinds[i].kind;
    return NULL;
}

// Controls that have an AVFoundation mode counterpart needing a lock before writing.
typedef struct { const char *range_kind; const char *auto_kind; } AutoKindEntry;
static const AutoKindEntry kAutoKinds[] = {
    { "focus_absolute",           "focus_auto"          },
    { "exposure_time_absolute",   "exposure_auto"       },
    { "white_balance_temperature","white_balance_auto"  },
};
static const int kAutoKindCount = (int)(sizeof(kAutoKinds) / sizeof(kAutoKinds[0]));

static const char *cmio_auto_kind_for_range_kind(const char *rangeKind) {
    for (int i = 0; i < kAutoKindCount; i++)
        if (strcmp(kAutoKinds[i].range_kind, rangeKind) == 0)
            return kAutoKinds[i].auto_kind;
    return NULL;
}

static const char *cmio_range_kind_for_auto_kind(const char *autoKind) {
    for (int i = 0; i < kAutoKindCount; i++)
        if (strcmp(kAutoKinds[i].auto_kind, autoKind) == 0)
            return kAutoKinds[i].range_kind;
    return NULL;
}

static uint32_t cmio_get_class(CMIOObjectID obj) {
    CMIOObjectPropertyAddress addr = {
        kCMIOObjectPropertyClass,
        kCMIOObjectPropertyScopeGlobal,
        kCMIOObjectPropertyElementMain
    };
    CMIOClassID cls = 0;
    UInt32 sz = sizeof(cls);
    CMIOObjectGetPropertyData(obj, &addr, 0, NULL, sz, &sz, &cls);
    return (uint32_t)cls;
}

// Find the CMIO device whose UID matches the AVCaptureDevice.uniqueID.
static CMIOObjectID cmio_find_device(NSString *uniqueID) {
    CMIOObjectPropertyAddress devAddr = {
        kCMIOHardwarePropertyDevices,
        kCMIOObjectPropertyScopeGlobal,
        kCMIOObjectPropertyElementMain
    };
    UInt32 dataSize = 0;
    if (CMIOObjectGetPropertyDataSize(kCMIOObjectSystemObject, &devAddr, 0, NULL, &dataSize) != noErr
        || dataSize == 0) return kCMIOObjectUnknown;

    CMIOObjectID *devs = malloc(dataSize);
    if (!devs) return kCMIOObjectUnknown;
    UInt32 outSize = dataSize;
    CMIOObjectGetPropertyData(kCMIOObjectSystemObject, &devAddr, 0, NULL, dataSize, &outSize, devs);
    UInt32 count = outSize / sizeof(CMIOObjectID);

    CMIOObjectPropertyAddress uidAddr = {
        kCMIODevicePropertyDeviceUID,
        kCMIOObjectPropertyScopeGlobal,
        kCMIOObjectPropertyElementMain
    };
    CMIOObjectID result = kCMIOObjectUnknown;
    for (UInt32 i = 0; i < count && result == kCMIOObjectUnknown; i++) {
        CFStringRef uid = NULL;
        UInt32 sz = sizeof(uid);
        if (CMIOObjectGetPropertyData(devs[i], &uidAddr, 0, NULL, sz, &sz, &uid) == noErr && uid) {
            if ([(__bridge NSString *)uid isEqualToString:uniqueID]) result = devs[i];
            CFRelease(uid);
        }
    }
    free(devs);
    return result;
}

// Return all CMIOObjectIDs owned by `parent`. Caller must free().
static CMIOObjectID *cmio_owned(CMIOObjectID parent, UInt32 *outCount) {
    CMIOObjectPropertyAddress addr = {
        kCMIOObjectPropertyOwnedObjects,
        kCMIOObjectPropertyScopeGlobal,
        kCMIOObjectPropertyElementMain
    };
    UInt32 dataSize = 0;
    if (CMIOObjectGetPropertyDataSize(parent, &addr, 0, NULL, &dataSize) != noErr || dataSize == 0) {
        *outCount = 0; return NULL;
    }
    CMIOObjectID *objs = malloc(dataSize);
    if (!objs) { *outCount = 0; return NULL; }
    UInt32 outSize = dataSize;
    if (CMIOObjectGetPropertyData(parent, &addr, 0, NULL, dataSize, &outSize, objs) != noErr) {
        free(objs); *outCount = 0; return NULL;
    }
    *outCount = outSize / sizeof(CMIOObjectID);
    return objs;
}

// Collect all feature-control objects reachable from `deviceID`
// (device-level objects + stream-level objects).
static NSArray<NSNumber *> *cmio_collect_controls(CMIOObjectID deviceID) {
    NSMutableArray *result = [NSMutableArray array];

    UInt32 n = 0;
    CMIOObjectID *devObjs = cmio_owned(deviceID, &n);
    if (!devObjs) return result;

    for (UInt32 i = 0; i < n; i++) {
        uint32_t cls = cmio_get_class(devObjs[i]);
        if (cmio_kind_for_class(cls)) {
            [result addObject:@(devObjs[i])];
        } else if (cls == kCMIOStreamClassID) {
            UInt32 sn = 0;
            CMIOObjectID *streamObjs = cmio_owned(devObjs[i], &sn);
            if (streamObjs) {
                for (UInt32 j = 0; j < sn; j++) {
                    if (cmio_kind_for_class(cmio_get_class(streamObjs[j])))
                        [result addObject:@(streamObjs[j])];
                }
                free(streamObjs);
            }
        }
    }
    free(devObjs);
    return result;
}

// Populate a range WcParamDesc from a CMIO feature control object.
static BOOL push_cmio_range(WcParamDesc *out, int *count, int capacity,
                              CMIOObjectID ctrl, const char *kind) {
    if (*count >= capacity) return NO;

    CMIOObjectPropertyAddress rangeAddr = {
        kCMIOFeatureControlPropertyNativeRange,
        kCMIOObjectPropertyScopeGlobal,
        kCMIOObjectPropertyElementMain
    };
    AudioValueRange range = {0, 0};
    UInt32 sz = sizeof(range);
    if (CMIOObjectGetPropertyData(ctrl, &rangeAddr, 0, NULL, sz, &sz, &range) != noErr)
        return NO;
    if (range.mMinimum >= range.mMaximum) return NO;

    CMIOObjectPropertyAddress valAddr = {
        kCMIOFeatureControlPropertyNativeValue,
        kCMIOObjectPropertyScopeGlobal,
        kCMIOObjectPropertyElementMain
    };
    Float32 cur = (Float32)range.mMinimum;
    sz = sizeof(cur);
    CMIOObjectGetPropertyData(ctrl, &valAddr, 0, NULL, sz, &sz, &cur);

    WcParamDesc *p = &out[(*count)++];
    memset(p, 0, sizeof(*p));
    strlcpy(p->kind, kind, WC_MAX_KIND);
    p->current  = (int)roundf(cur);
    p->is_range = 1;
    p->min      = (int)range.mMinimum;
    p->max      = (int)range.mMaximum;
    p->step     = 1;
    return YES;
}

// ---------------------------------------------------------------------------
// Discrete option helpers
// ---------------------------------------------------------------------------

static void push_option(WcParamDesc *p, int value, const char *label) {
    if (p->num_options >= WC_MAX_OPTIONS) return;
    p->options[p->num_options].value = value;
    strlcpy(p->options[p->num_options].label, label, WC_MAX_LABEL);
    p->num_options++;
}

// Expose the CMIO AutomaticManual property (0=manual, 1=auto) as a discrete param.
static BOOL push_cmio_auto_manual(WcParamDesc *out, int *count, int capacity,
                                   CMIOObjectID ctrl, const char *autoKind) {
    if (*count >= capacity) return NO;
    CMIOObjectPropertyAddress addr = {
        kCMIOFeatureControlPropertyAutomaticManual,
        kCMIOObjectPropertyScopeGlobal,
        kCMIOObjectPropertyElementMain
    };
    UInt32 val = 0, sz = sizeof(val);
    if (CMIOObjectGetPropertyData(ctrl, &addr, 0, NULL, sz, &sz, &val) != noErr) return NO;

    WcParamDesc *p = &out[(*count)++];
    memset(p, 0, sizeof(*p));
    strlcpy(p->kind, autoKind, WC_MAX_KIND);
    p->current = (int)val;
    push_option(p, 0, "Manual");
    push_option(p, 1, "Auto");
    return YES;
}

// ---------------------------------------------------------------------------
// C interface — session management
// ---------------------------------------------------------------------------

int wc_list_devices(WcDeviceInfo *out, int capacity) {
    if (!out || capacity <= 0) return 0;

    NSArray<AVCaptureDeviceType> *types;
    if (@available(macOS 14.0, *)) {
        types = @[AVCaptureDeviceTypeBuiltInWideAngleCamera,
                  AVCaptureDeviceTypeExternal];
    } else {
#pragma clang diagnostic push
#pragma clang diagnostic ignored "-Wdeprecated-declarations"
        types = @[AVCaptureDeviceTypeBuiltInWideAngleCamera,
                  AVCaptureDeviceTypeExternalUnknown];
#pragma clang diagnostic pop
    }

    AVCaptureDeviceDiscoverySession *ds =
        [AVCaptureDeviceDiscoverySession
            discoverySessionWithDeviceTypes:types
                                  mediaType:AVMediaTypeVideo
                                   position:AVCaptureDevicePositionUnspecified];

    int count = 0;
    for (AVCaptureDevice *dev in ds.devices) {
        if (count >= capacity) break;
        strlcpy(out[count].unique_id, dev.uniqueID.UTF8String      ?: "", WC_MAX_STR);
        strlcpy(out[count].name,      dev.localizedName.UTF8String ?: "", WC_MAX_STR);
        count++;
    }
    return count;
}

void *wc_open_session(const char *unique_id) {
    NSString *uid = [NSString stringWithUTF8String:unique_id];
    AVCaptureDevice *device = [AVCaptureDevice deviceWithUniqueID:uid];
    if (!device) return NULL;

    NSError *error = nil;
    AVCaptureDeviceInput *input =
        [AVCaptureDeviceInput deviceInputWithDevice:device error:&error];
    if (!input) return NULL;

    dispatch_queue_t q =
        dispatch_queue_create("bird.avfoundation.capture", DISPATCH_QUEUE_SERIAL);
    WcFrameDelegate *delegate = [[WcFrameDelegate alloc] init];

    AVCaptureVideoDataOutput *output = [[AVCaptureVideoDataOutput alloc] init];
    output.videoSettings = @{
        (id)kCVPixelBufferPixelFormatTypeKey: @(kCVPixelFormatType_32BGRA)
    };
    output.alwaysDiscardsLateVideoFrames = YES;
    [output setSampleBufferDelegate:delegate queue:q];

    AVCaptureSession *session = [[AVCaptureSession alloc] init];
    session.sessionPreset = AVCaptureSessionPreset1280x720;
    if (![session canAddInput:input] || ![session canAddOutput:output])
        return NULL;

    [session addInput:input];
    [session addOutput:output];
    [session startRunning];

    WcSessionHandle *handle = [[WcSessionHandle alloc] init];
    handle.session      = session;
    handle.device       = device;
    handle.delegate     = delegate;
    handle.captureQueue = q;

    CMIOObjectID cmioID = cmio_find_device(device.uniqueID);
    if (cmioID != kCMIOObjectUnknown) [handle setCmioDeviceID:(uint32_t)cmioID];

    // Lock AVFoundation modes once so they never interfere with CMIO writes.
    // All focus/exposure/white_balance control goes exclusively through CMIO.
    NSError *lockErr = nil;
    if ([device lockForConfiguration:&lockErr]) {
        if ([device isFocusModeSupported:AVCaptureFocusModeLocked])
            device.focusMode = AVCaptureFocusModeLocked;
        if ([device isExposureModeSupported:AVCaptureExposureModeLocked])
            device.exposureMode = AVCaptureExposureModeLocked;
        if ([device isWhiteBalanceModeSupported:AVCaptureWhiteBalanceModeLocked])
            device.whiteBalanceMode = AVCaptureWhiteBalanceModeLocked;
        [device unlockForConfiguration];
    }

    return (__bridge_retained void *)handle;
}

void wc_close_session(void *handle) {
    if (!handle) return;
    WcSessionHandle *h = (__bridge_transfer WcSessionHandle *)handle;
    [h.session stopRunning];
}

int wc_capture_frame(void *handle, uint8_t **out_data, size_t *out_size) {
    if (!handle || !out_data || !out_size) return -1;
    WcSessionHandle *h = (__bridge WcSessionHandle *)handle;
    NSData *jpeg = [h.delegate encodeLatestFrameAsJPEG];
    if (!jpeg || jpeg.length == 0) return -1;

    uint8_t *buf = (uint8_t *)malloc(jpeg.length);
    if (!buf) return -1;
    memcpy(buf, jpeg.bytes, jpeg.length);
    *out_data = buf;
    *out_size = jpeg.length;
    return 0;
}

void wc_free_frame(uint8_t *data) { free(data); }

// ---------------------------------------------------------------------------
// C interface — parameter enumeration
// ---------------------------------------------------------------------------

int wc_get_parameters(void *handle, WcParamDesc *out, int capacity) {
    if (!handle || !out || capacity <= 0) return 0;
    WcSessionHandle *h = (__bridge WcSessionHandle *)handle;
    AVCaptureDevice *dev = h.device;
    if (!dev) return 0;

    int count = 0;

    // --- CMIOHardware feature controls ---
    // For focus/exposure/white_balance: expose auto/manual toggle then the range value.
    // This replaces AVFoundation's discrete mode params so there's a single control path.

    CMIOObjectID cmioID = (CMIOObjectID)[h cmioDeviceID];
    if (cmioID != kCMIOObjectUnknown) {
        NSArray<NSNumber *> *controls = cmio_collect_controls(cmioID);
        for (NSNumber *objNum in controls) {
            CMIOObjectID ctrl = (CMIOObjectID)objNum.unsignedIntValue;
            const char *kind = cmio_kind_for_class(cmio_get_class(ctrl));
            if (!kind) continue;
            // Auto/manual toggle (only for controls that have an AVFoundation counterpart).
            const char *autoKind = cmio_auto_kind_for_range_kind(kind);
            if (autoKind) push_cmio_auto_manual(out, &count, capacity, ctrl, autoKind);
            // Range value.
            push_cmio_range(out, &count, capacity, ctrl, kind);
        }
    }

    return count;
}

// ---------------------------------------------------------------------------
// C interface — parameter setting
// ---------------------------------------------------------------------------

// Lock the relevant AVFoundation mode before writing a CMIO hardware value.
// AVFoundation will otherwise keep the hardware in auto mode and reject CMIO writes.
int wc_set_parameter(void *handle, const char *kind, int value) {
    if (!handle || !kind) return -1;
    WcSessionHandle *h = (__bridge WcSessionHandle *)handle;
    if (![h cmioDeviceID]) return -1;

    CMIOObjectID cmioID = (CMIOObjectID)[h cmioDeviceID];
    if (cmioID == kCMIOObjectUnknown) return -1;

    const char *rangeKind = cmio_range_kind_for_auto_kind(kind);
    const char *lookupKind = rangeKind ? rangeKind : kind;

    NSArray<NSNumber *> *controls = cmio_collect_controls(cmioID);
    CMIOObjectID ctrlObj = kCMIOObjectUnknown;
    for (NSNumber *objNum in controls) {
        CMIOObjectID obj = (CMIOObjectID)objNum.unsignedIntValue;
        const char *k = cmio_kind_for_class(cmio_get_class(obj));
        if (k && strcmp(k, lookupKind) == 0) { ctrlObj = obj; break; }
    }
    if (ctrlObj == kCMIOObjectUnknown) return -1;

    CMIOObjectPropertyAddress autoAddr = {
        kCMIOFeatureControlPropertyAutomaticManual,
        kCMIOObjectPropertyScopeGlobal,
        kCMIOObjectPropertyElementMain
    };

    // Auto/manual toggle — just set AutomaticManual directly.
    if (rangeKind) {
        UInt32 v = (UInt32)value;
        return CMIOObjectSetPropertyData(ctrlObj, &autoAddr, 0, NULL, sizeof(v), &v) == noErr
               ? 0 : -1;
    }

    // Absolute range value — disable auto, ensure on, then write value.
    UInt32 manual = 0;
    CMIOObjectSetPropertyData(ctrlObj, &autoAddr, 0, NULL, sizeof(manual), &manual);

    CMIOObjectPropertyAddress onOffAddr = {
        kCMIOFeatureControlPropertyOnOff,
        kCMIOObjectPropertyScopeGlobal,
        kCMIOObjectPropertyElementMain
    };
    UInt32 on = 1;
    CMIOObjectSetPropertyData(ctrlObj, &onOffAddr, 0, NULL, sizeof(on), &on);

    Float32 val = (Float32)value;

    CMIOObjectPropertyAddress nativeAddr = {
        kCMIOFeatureControlPropertyNativeValue,
        kCMIOObjectPropertyScopeGlobal,
        kCMIOObjectPropertyElementMain
    };
    if (CMIOObjectSetPropertyData(ctrlObj, &nativeAddr, 0, NULL, sizeof(val), &val) == noErr)
        return 0;

    CMIOObjectPropertyAddress absAddr = {
        kCMIOFeatureControlPropertyAbsoluteValue,
        kCMIOObjectPropertyScopeGlobal,
        kCMIOObjectPropertyElementMain
    };
    return CMIOObjectSetPropertyData(ctrlObj, &absAddr, 0, NULL, sizeof(val), &val) == noErr
           ? 0 : -1;
}
