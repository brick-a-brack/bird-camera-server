@import AVFoundation;
@import ImageIO;
@import Foundation;
@import CoreMediaIO;

#include "bridge.h"
#include <IOKit/IOKitLib.h>
#include <IOKit/usb/IOUSBLib.h>
#include <IOKit/usb/USB.h>
#include <stdlib.h>
#include <string.h>
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
// UVC direct I/O layer (IOKit ControlRequest)
// ---------------------------------------------------------------------------

#define UVC_SET_CUR            0x01
#define UVC_CS_INTERFACE       0x24
#define UVC_VC_INPUT_TERMINAL  0x02
#define UVC_VC_PROCESSING_UNIT 0x05
#define UVC_ITT_CAMERA         0x0201

// Walk the USB configuration descriptor to find the VideoControl interface number,
// Processing Unit ID, and Camera Terminal ID.
static int uvc_parse_config(IOUSBDeviceInterface **dev,
                             uint8_t *outVCIf, uint8_t *outPU, uint8_t *outCT) {
    IOUSBConfigurationDescriptorPtr cfg = NULL;
    if ((*dev)->GetConfigurationDescriptorPtr(dev, 0, &cfg) != kIOReturnSuccess || !cfg) return -1;

    uint8_t  *buf   = (uint8_t *)cfg;
    uint16_t  total = cfg->wTotalLength;
    *outVCIf = 0xFF; *outPU = 0; *outCT = 0;

    BOOL     inVC  = NO;
    uint16_t off   = 0;
    while (off + 2 <= total) {
        uint8_t bLen  = buf[off];
        uint8_t bType = buf[off + 1];
        if (bLen < 2 || (uint16_t)(off + bLen) > total) break;

        if (bType == kUSBInterfaceDesc && bLen >= 9) {
            inVC = (buf[off + 5] == 0x0E && buf[off + 6] == 0x01);
            if (inVC) *outVCIf = buf[off + 2];
        } else if (bType == UVC_CS_INTERFACE && inVC && bLen >= 4) {
            uint8_t sub = buf[off + 2];
            if (sub == UVC_VC_PROCESSING_UNIT && !*outPU)
                *outPU = buf[off + 3];
            else if (sub == UVC_VC_INPUT_TERMINAL && bLen >= 8 && !*outCT) {
                uint16_t termType = (uint16_t)(buf[off + 4] | (buf[off + 5] << 8));
                if (termType == UVC_ITT_CAMERA)
                    *outCT = buf[off + 3];
            }
        }
        off += bLen;
    }
    NSLog(@"[uvc] parse_config: vcIf=%u PU=%u CT=%u", *outVCIf, *outPU, *outCT);
    return (*outPU || *outCT) ? 0 : -1;
}

// Open the VideoControl interface for the camera identified by AVFoundation uniqueID.
// Uses IOUSBInterfaceInterface::ControlRequest instead of IOUSBDeviceInterface::DeviceRequest.
// The kernel allows class requests through the interface object even when AVFoundation holds
// the device open (USBInterfaceOpen returns kIOReturnExclusiveAccess = 0xE00002C5 — acceptable).
static IOUSBInterfaceInterface190 **uvc_open_vc_interface(NSString *uniqueID,
                                                           uint8_t *outVCIf,
                                                           uint8_t *outPU,
                                                           uint8_t *outCT) {
    *outVCIf = 0xFF; *outPU = 0; *outCT = 0;
    NSLog(@"[uvc] open uniqueID=%@", uniqueID);

    // Built-in cameras and non-USB devices have a different uniqueID format — skip them.
    if (![uniqueID hasPrefix:@"0x"] || uniqueID.length < 10) {
        NSLog(@"[uvc] not a USB uniqueID, skipping");
        return NULL;
    }
    NSScanner *sc = [NSScanner scannerWithString:uniqueID];
    [sc scanString:@"0x" intoString:nil];
    unsigned long long combined = 0;
    if (![sc scanHexLongLong:&combined]) return NULL;
    uint32_t locationID = (uint32_t)(combined >> 32);
    NSLog(@"[uvc] locationID=0x%08X", locationID);

    // 1. Find IOUSBDevice by locationID.
    io_iterator_t devIter = 0;
    kern_return_t kr = IOServiceGetMatchingServices(kIOMasterPortDefault,
                           IOServiceMatching(kIOUSBDeviceClassName), &devIter);
    if (kr != kIOReturnSuccess) return NULL;
    io_service_t devSvc = 0, svc;
    while ((svc = IOIteratorNext(devIter))) {
        CFNumberRef locRef = IORegistryEntryCreateCFProperty(svc, CFSTR("locationID"),
                                                              kCFAllocatorDefault, 0);
        if (locRef) {
            uint32_t loc = 0; CFNumberGetValue(locRef, kCFNumberSInt32Type, &loc); CFRelease(locRef);
            if (loc == locationID) { devSvc = svc; break; }
        }
        IOObjectRelease(svc);
    }
    IOObjectRelease(devIter);
    if (!devSvc) { NSLog(@"[uvc] device not found"); return NULL; }

    // 2. Get IOUSBDeviceInterface (needed for config descriptor + interface iterator).
    IOCFPlugInInterface **devPlugin = NULL; SInt32 score = 0;
    kr = IOCreatePlugInInterfaceForService(devSvc, kIOUSBDeviceUserClientTypeID,
                                            kIOCFPlugInInterfaceID, &devPlugin, &score);
    IOObjectRelease(devSvc);
    if (kr != kIOReturnSuccess || !devPlugin) return NULL;
    IOUSBDeviceInterface **dev = NULL;
    HRESULT hr = (*devPlugin)->QueryInterface(devPlugin,
                     CFUUIDGetUUIDBytes(kIOUSBDeviceInterfaceID), (LPVOID *)&dev);
    (*devPlugin)->Release(devPlugin);
    if (hr || !dev) return NULL;

    // 3. Parse config descriptor for PU/CT unit IDs and VC interface number.
    uvc_parse_config(dev, outVCIf, outPU, outCT);

    // 4. Find the VideoControl interface service.
    IOUSBFindInterfaceRequest ifReq = {
        .bInterfaceClass    = 0x0E,
        .bInterfaceSubClass = 0x01,
        .bInterfaceProtocol = kIOUSBFindInterfaceDontCare,
        .bAlternateSetting  = kIOUSBFindInterfaceDontCare,
    };
    io_iterator_t ifIter = 0;
    kr = (*dev)->CreateInterfaceIterator(dev, &ifReq, &ifIter);
    (*dev)->Release(dev);
    if (kr != kIOReturnSuccess) return NULL;
    io_service_t vcSvc = IOIteratorNext(ifIter);
    IOObjectRelease(ifIter);
    if (!vcSvc) return NULL;

    // 5. Get IOUSBInterfaceInterface190 for the VC interface.
    IOCFPlugInInterface **ifPlugin = NULL;
    kr = IOCreatePlugInInterfaceForService(vcSvc, kIOUSBInterfaceUserClientTypeID,
                                            kIOCFPlugInInterfaceID, &ifPlugin, &score);
    IOObjectRelease(vcSvc);
    if (kr != kIOReturnSuccess || !ifPlugin) return NULL;
    IOUSBInterfaceInterface190 **intf = NULL;
    hr = (*ifPlugin)->QueryInterface(ifPlugin,
             CFUUIDGetUUIDBytes(kIOUSBInterfaceInterfaceID190), (LPVOID *)&intf);
    (*ifPlugin)->Release(ifPlugin);
    if (hr || !intf) return NULL;

    // 6. Open the interface. kIOReturnExclusiveAccess is acceptable — ControlRequest still works.
    IOReturn openKr = (*intf)->USBInterfaceOpen(intf);
    NSLog(@"[uvc] USBInterfaceOpen kr=0x%X", openKr);
    if (openKr != kIOReturnSuccess && openKr != kIOReturnExclusiveAccess) {
        (*intf)->Release(intf);
        return NULL;
    }
    return intf;
}

static int uvc_set_cur(IOUSBInterfaceInterface190 **intf, uint8_t unitID, uint8_t selector,
                        uint8_t ifNum, void *data, uint16_t len) {
    IOUSBDevRequest req;
    memset(&req, 0, sizeof(req));
    req.bmRequestType = USBmakebmRequestType(kUSBOut, kUSBClass, kUSBInterface);
    req.bRequest      = UVC_SET_CUR;
    req.wValue        = (uint16_t)(selector << 8);
    req.wIndex        = (uint16_t)((unitID << 8) | ifNum);
    req.wLength       = len;
    req.pData         = data;
    IOReturn kr = (*intf)->ControlRequest(intf, 0, &req);
    NSLog(@"[uvc] SET_CUR unit=%u sel=0x%02X if=%u len=%u -> kr=0x%X", unitID, selector, ifNum, len, kr);
    return (kr == kIOReturnSuccess) ? 0 : -1;
}

// request: GET_CUR=0x81, GET_MIN=0x82, GET_MAX=0x83, GET_RES=0x84
static int uvc_get_req(IOUSBInterfaceInterface190 **intf, uint8_t request, uint8_t unitID,
                        uint8_t selector, uint8_t ifNum, void *data, uint16_t len) {
    IOUSBDevRequest req;
    memset(&req, 0, sizeof(req));
    req.bmRequestType = USBmakebmRequestType(kUSBIn, kUSBClass, kUSBInterface);
    req.bRequest      = request;
    req.wValue        = (uint16_t)(selector << 8);
    req.wIndex        = (uint16_t)((unitID << 8) | ifNum);
    req.wLength       = len;
    req.pData         = data;
    IOReturn kr = (*intf)->ControlRequest(intf, 0, &req);
    return (kr == kIOReturnSuccess) ? 0 : -1;
}

// Read a 1/2/4-byte UVC value into a signed int32_t (little-endian on the wire).
// 1-byte values are unsigned; 2-byte signed int16; 4-byte signed int32.
static int uvc_read_int(IOUSBInterfaceInterface190 **intf, uint8_t request, uint8_t unitID,
                         uint8_t selector, uint8_t ifNum, uint8_t size, int32_t *out) {
    uint8_t buf[4] = {0};
    if (uvc_get_req(intf, request, unitID, selector, ifNum, buf, size) != 0) return -1;
    switch (size) {
        case 1: *out = (int32_t)(uint8_t)buf[0]; break;
        case 2: { int16_t v; memcpy(&v, buf, 2); *out = (int32_t)v; break; }
        case 4: { int32_t v; memcpy(&v, buf, 4); *out = v; break; }
        default: return -1;
    }
    return 0;
}

// ---------------------------------------------------------------------------
// Unified control descriptor table
// ---------------------------------------------------------------------------

typedef enum { CTRL_RANGE, CTRL_BOOL_MANUAL_AUTO, CTRL_BOOL_OFF_ON, CTRL_ENUM_PLF } CtrlPresentation;
typedef enum { AVF_NONE, AVF_FOCUS, AVF_WHITE_BALANCE, AVF_EXPOSURE } CtrlAvfSync;

typedef struct {
    const char      *kind;
    uint8_t          uvc_selector;
    BOOL             uvc_is_pu;       // YES = Processing Unit, NO = Camera Terminal
    uint8_t          uvc_size;        // bytes on the wire: 1, 2, or 4
    uint32_t         cmio_class;      // CMIO class for range reads; 0 = UVC-only
    uint32_t         cmio_auto_class; // CMIO class to cooperate with on write; 0 = none
    CtrlPresentation presentation;
    CtrlAvfSync      avf_sync;
    const char      *guarded_by;      // kind of the auto control that gates this one; NULL = always editable
} ControlDesc;

// exposure_auto uses a non-standard UVC value mapping:
//   read : UVC 1 (manual) → logical 0, UVC 8 (aperture priority) → logical 1
//   write: logical 0 → UVC 1, logical 1 → UVC 8
// This is handled by name in uvcWriteKind:value: and in wc_get_parameters.

static const ControlDesc kControls[] = {
    // Processing Unit controls
    //  kind                        sel    PU?   sz  cmio_class                               cmio_auto_class                   presentation           avf_sync        guarded_by
    { "backlight_compensation",    0x01, YES,  2, kCMIOBacklightCompensationControlClassID, 0,                                CTRL_RANGE,            AVF_NONE,          NULL                    },
    { "brightness",                0x02, YES,  2, kCMIOBrightnessControlClassID,            0,                                CTRL_RANGE,            AVF_NONE,          NULL                    },
    { "contrast",                  0x03, YES,  2, kCMIOContrastControlClassID,              0,                                CTRL_RANGE,            AVF_NONE,          NULL                    },
    { "gain",                      0x04, YES,  2, kCMIOGainControlClassID,                  0,                                CTRL_RANGE,            AVF_NONE,          NULL                    },
    { "power_line_frequency",      0x05, YES,  1, 0,                                        0,                                CTRL_ENUM_PLF,         AVF_NONE,          NULL                    },
    { "hue",                       0x06, YES,  2, kCMIOHueControlClassID,                   0,                                CTRL_RANGE,            AVF_NONE,          NULL                    },
    { "saturation",                0x07, YES,  2, kCMIOSaturationControlClassID,            0,                                CTRL_RANGE,            AVF_NONE,          NULL                    },
    { "sharpness",                 0x08, YES,  2, kCMIOSharpnessControlClassID,             0,                                CTRL_RANGE,            AVF_NONE,          NULL                    },
    { "gamma",                     0x09, YES,  2, 0,                                        0,                                CTRL_RANGE,            AVF_NONE,          NULL                    },
    { "white_balance_temperature", 0x0A, YES,  2, kCMIOTemperatureControlClassID,           0,                                CTRL_RANGE,            AVF_NONE,          "white_balance_auto"    },
    { "white_balance_auto",        0x0B, YES,  1, 0,                                        kCMIOTemperatureControlClassID,   CTRL_BOOL_MANUAL_AUTO, AVF_WHITE_BALANCE, NULL                    },
    { "color_enable",              0x0C, YES,  1, 0,                                        0,                                CTRL_BOOL_OFF_ON,      AVF_NONE,          NULL                    },
    { "hue_auto",                  0x0F, YES,  1, 0,                                        0,                                CTRL_BOOL_MANUAL_AUTO, AVF_NONE,          NULL                    },
    // Camera Terminal controls
    { "exposure_auto",             0x02, NO,   1, 0,                                        kCMIOExposureControlClassID,      CTRL_BOOL_MANUAL_AUTO, AVF_EXPOSURE,      NULL                    },
    { "exposure_time_absolute",    0x04, NO,   4, kCMIOExposureControlClassID,              0,                                CTRL_RANGE,            AVF_NONE,          "exposure_auto"         },
    { "focus_absolute",            0x06, NO,   2, kCMIOFocusControlClassID,                 0,                                CTRL_RANGE,            AVF_NONE,          "focus_auto"            },
    { "focus_auto",                0x08, NO,   1, 0,                                        0,                                CTRL_BOOL_MANUAL_AUTO, AVF_FOCUS,         NULL                    },
    { "iris_absolute",             0x09, NO,   2, 0,                                        0,                                CTRL_RANGE,            AVF_NONE,          NULL                    },
    { "zoom_absolute",             0x0B, NO,   2, kCMIOZoomControlClassID,                  0,                                CTRL_RANGE,            AVF_NONE,          NULL                    },
    { "pan_absolute",              0x0D, NO,   4, 0,                                        0,                                CTRL_RANGE,            AVF_NONE,          NULL                    },
    { "tilt_absolute",             0x0E, NO,   4, 0,                                        0,                                CTRL_RANGE,            AVF_NONE,          NULL                    },
};
static const int kControlCount = (int)(sizeof(kControls) / sizeof(kControls[0]));

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
- (void)setUvcInterface:(IOUSBInterfaceInterface190 **)intf
            vcInterface:(uint8_t)vcIf
                     pu:(uint8_t)pu
                     ct:(uint8_t)ct;
- (BOOL)uvcAvailable;
- (BOOL)uvcHasCT;
- (BOOL)uvcHasPU;
// Read any UVC control value via GET_CUR / GET_MIN / GET_MAX / GET_RES.
- (int)uvcGetSelector:(uint8_t)selector request:(uint8_t)req isPU:(BOOL)isPU out:(int32_t *)out size:(uint8_t)size;
// Write a UVC control by kind name (handles exposure_auto AE-mode mapping internally).
- (int)uvcWriteKind:(const char *)kind value:(int32_t)value;
@end

@implementation WcSessionHandle {
    uint32_t                    _cmioDeviceID;
    IOUSBInterfaceInterface190 **_uvcIF;
    uint8_t                     _uvcVCIf;
    uint8_t                     _uvcPU;
    uint8_t                     _uvcCT;
}

- (void)dealloc {
    if (_uvcIF) {
        (*_uvcIF)->USBInterfaceClose(_uvcIF);
        (*_uvcIF)->Release(_uvcIF);
        _uvcIF = NULL;
    }
}

- (void)setCmioDeviceID:(uint32_t)devID { _cmioDeviceID = devID; }
- (uint32_t)cmioDeviceID { return _cmioDeviceID; }

- (void)setUvcInterface:(IOUSBInterfaceInterface190 **)intf
            vcInterface:(uint8_t)vcIf
                     pu:(uint8_t)pu
                     ct:(uint8_t)ct {
    _uvcIF   = intf;
    _uvcVCIf = vcIf;
    _uvcPU   = pu;
    _uvcCT   = ct;
}

- (BOOL)uvcAvailable { return _uvcIF != NULL; }
- (BOOL)uvcHasCT     { return _uvcIF != NULL && _uvcCT != 0; }
- (BOOL)uvcHasPU     { return _uvcIF != NULL && _uvcPU != 0; }

- (int)uvcGetSelector:(uint8_t)selector request:(uint8_t)req isPU:(BOOL)isPU out:(int32_t *)out size:(uint8_t)size {
    if (!_uvcIF) return -1;
    uint8_t unitID = isPU ? _uvcPU : _uvcCT;
    if (!unitID) return -1;
    return uvc_read_int(_uvcIF, req, unitID, selector, _uvcVCIf, size, out);
}

- (int)uvcWriteKind:(const char *)kind value:(int32_t)value {
    // Find the descriptor.
    const ControlDesc *d = NULL;
    for (int i = 0; i < kControlCount; i++) {
        if (strcmp(kControls[i].kind, kind) == 0) { d = &kControls[i]; break; }
    }
    if (!d) return -1;

    uint8_t unitID = d->uvc_is_pu ? _uvcPU : _uvcCT;
    if (!unitID) return -1;

    int32_t uvcVal = value;

    // exposure_auto: logical 0 (manual) → UVC AE mode 1, logical 1 (auto) → UVC AE mode 8 (aperture priority).
    if (strcmp(kind, "exposure_auto") == 0)
        uvcVal = (value == 0) ? 1 : 8;

    uint8_t buf[4] = {0};
    switch (d->uvc_size) {
        case 1: buf[0] = (uint8_t)uvcVal; break;
        case 2: { uint16_t v = (uint16_t)(int16_t)uvcVal; memcpy(buf, &v, 2); break; }
        case 4: { uint32_t v = (uint32_t)uvcVal;           memcpy(buf, &v, 4); break; }
        default: return -1;
    }
    NSLog(@"[uvc] write kind=%s value=%d bytes=[%02X %02X %02X %02X]",
          kind, (int)uvcVal, buf[0], buf[1], buf[2], buf[3]);
    return uvc_set_cur(_uvcIF, unitID, d->uvc_selector, _uvcVCIf, buf, d->uvc_size);
}
@end

// ---------------------------------------------------------------------------
// CMIO helpers (read-only — used for range enumeration)
// ---------------------------------------------------------------------------

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

// Build a dictionary mapping CMIO class ID → CMIOObjectID for all feature controls on the device.
static NSDictionary<NSNumber *, NSNumber *> *cmio_build_class_map(CMIOObjectID deviceID) {
    NSMutableDictionary *map = [NSMutableDictionary dictionary];

    UInt32 n = 0;
    CMIOObjectID *devObjs = cmio_owned(deviceID, &n);
    if (!devObjs) return map;

    for (UInt32 i = 0; i < n; i++) {
        uint32_t cls = cmio_get_class(devObjs[i]);
        if (cls == kCMIOStreamClassID) {
            UInt32 sn = 0;
            CMIOObjectID *streamObjs = cmio_owned(devObjs[i], &sn);
            if (streamObjs) {
                for (UInt32 j = 0; j < sn; j++) {
                    uint32_t sCls = cmio_get_class(streamObjs[j]);
                    map[@(sCls)] = @(streamObjs[j]);
                }
                free(streamObjs);
            }
        } else {
            map[@(cls)] = @(devObjs[i]);
        }
    }
    free(devObjs);
    return map;
}

// Try to read a range from a CMIO feature control into p. Returns YES on success.
static BOOL cmio_read_range(WcParamDesc *p, CMIOObjectID ctrl) {
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

    p->current = (int)roundf(cur);
    p->is_range = 1;
    p->min      = (int)roundf((Float32)range.mMinimum);
    p->max      = (int)roundf((Float32)range.mMaximum);
    p->step     = 1;
    return YES;
}

// Try to set kCMIOFeatureControlPropertyAutomaticManual for a control of the given class.
static BOOL cmio_set_auto_manual(CMIOObjectID deviceID, uint32_t controlClassID, UInt32 value) {
    NSDictionary<NSNumber*, NSNumber*> *map = cmio_build_class_map(deviceID);
    NSNumber *ctrlNum = map[@(controlClassID)];
    if (!ctrlNum) return NO;
    CMIOObjectID ctrl = (CMIOObjectID)ctrlNum.unsignedIntValue;

    CMIOObjectPropertyAddress addr = {
        kCMIOFeatureControlPropertyAutomaticManual,
        kCMIOObjectPropertyScopeGlobal,
        kCMIOObjectPropertyElementMain
    };
    Boolean settable = NO;
    CMIOObjectIsPropertySettable(ctrl, &addr, &settable);
    NSLog(@"[cmio] AutomaticManual settable=%d for class=0x%X", (int)settable, controlClassID);
    if (!settable) return NO;
    UInt32 sz = sizeof(value);
    OSStatus err = CMIOObjectSetPropertyData(ctrl, &addr, 0, NULL, sz, &value);
    NSLog(@"[cmio] set AutomaticManual=%u -> err=%d", value, (int)err);
    return (err == noErr);
}

// ---------------------------------------------------------------------------
// Discrete option helper
// ---------------------------------------------------------------------------

static void push_option(WcParamDesc *p, int value, const char *label) {
    if (p->num_options >= WC_MAX_OPTIONS) return;
    p->options[p->num_options].value = value;
    strlcpy(p->options[p->num_options].label, label, WC_MAX_LABEL);
    p->num_options++;
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
        dispatch_queue_create("toucan.avfoundation.capture", DISPATCH_QUEUE_SERIAL);
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

    uint8_t vcIf = 0, pu = 0, ct = 0;
    IOUSBInterfaceInterface190 **uvcIF = uvc_open_vc_interface(device.uniqueID, &vcIf, &pu, &ct);
    if (uvcIF) [handle setUvcInterface:uvcIF vcInterface:vcIf pu:pu ct:ct];

    return (__bridge_retained void *)handle;
}

void wc_close_session(void *handle) {
    if (!handle) return;
    WcSessionHandle *h = (__bridge_transfer WcSessionHandle *)handle;
    [h.session stopRunning];
    // WcSessionHandle dealloc closes the UVC interface.
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
    if (!h.device) return 0;

    // Build CMIO class → control-object map once for the whole enumeration.
    NSDictionary<NSNumber*, NSNumber*> *cmioMap = nil;
    CMIOObjectID cmioID = (CMIOObjectID)[h cmioDeviceID];
    if (cmioID != kCMIOObjectUnknown)
        cmioMap = cmio_build_class_map(cmioID);

    int count = 0;
    for (int i = 0; i < kControlCount && count < capacity; i++) {
        const ControlDesc *d = &kControls[i];
        WcParamDesc *p = &out[count];
        memset(p, 0, sizeof(*p));
        strlcpy(p->kind, d->kind, WC_MAX_KIND);
        BOOL emitted = NO;

        // --- Range controls: try CMIO first, fall back to UVC GET_CUR/MIN/MAX ---
        if (d->presentation == CTRL_RANGE) {
            if (d->cmio_class && cmioMap) {
                NSNumber *ctrlNum = cmioMap[@(d->cmio_class)];
                if (ctrlNum)
                    emitted = cmio_read_range(p, (CMIOObjectID)ctrlNum.unsignedIntValue);
            }
            if (!emitted && (d->uvc_is_pu ? [h uvcHasPU] : [h uvcHasCT])) {
                int32_t cur = 0, minV = 0, maxV = 0, res = 1;
                if ([h uvcGetSelector:d->uvc_selector request:0x81 isPU:d->uvc_is_pu out:&cur  size:d->uvc_size] == 0 &&
                    [h uvcGetSelector:d->uvc_selector request:0x82 isPU:d->uvc_is_pu out:&minV size:d->uvc_size] == 0 &&
                    [h uvcGetSelector:d->uvc_selector request:0x83 isPU:d->uvc_is_pu out:&maxV size:d->uvc_size] == 0 &&
                    minV < maxV) {
                    [h uvcGetSelector:d->uvc_selector request:0x84 isPU:d->uvc_is_pu out:&res size:d->uvc_size];
                    p->current  = (int)cur;
                    p->is_range = 1;
                    p->min      = (int)minV;
                    p->max      = (int)maxV;
                    p->step     = (res > 0) ? (int)res : 1;
                    emitted = YES;
                }
            }
        }

        // --- Discrete controls: always read from UVC for accurate hardware state ---
        else if (d->uvc_is_pu ? [h uvcHasPU] : [h uvcHasCT]) {
            int32_t cur = 0;
            if ([h uvcGetSelector:d->uvc_selector request:0x81 isPU:d->uvc_is_pu out:&cur size:d->uvc_size] == 0) {
                // exposure_auto: UVC AE mode 1=manual → logical 0, otherwise → logical 1.
                if (strcmp(d->kind, "exposure_auto") == 0)
                    cur = (cur == 1) ? 0 : 1;
                p->current = (int)cur;

                switch (d->presentation) {
                    case CTRL_BOOL_MANUAL_AUTO:
                        push_option(p, 0, "Manual");
                        push_option(p, 1, "Auto");
                        emitted = YES;
                        break;
                    case CTRL_BOOL_OFF_ON:
                        push_option(p, 0, "Off");
                        push_option(p, 1, "On");
                        emitted = YES;
                        break;
                    case CTRL_ENUM_PLF:
                        push_option(p, 0, "Disabled");
                        push_option(p, 1, "50 Hz");
                        push_option(p, 2, "60 Hz");
                        emitted = YES;
                        break;
                    case CTRL_RANGE:
                        break; // handled above
                }
            }
        }

        if (emitted) count++;
    }

    // Remove range controls that are locked because their linked auto mode is active.
    // A control is suppressed when guarded_by names an auto control whose current value is 1 (Auto).
    int out_count = 0;
    for (int i = 0; i < count; i++) {
        const ControlDesc *d = NULL;
        for (int j = 0; j < kControlCount; j++) {
            if (strcmp(kControls[j].kind, out[i].kind) == 0) { d = &kControls[j]; break; }
        }
        BOOL suppress = NO;
        if (d && d->guarded_by) {
            for (int j = 0; j < count; j++) {
                if (strcmp(out[j].kind, d->guarded_by) == 0) {
                    suppress = (out[j].current == 1);
                    break;
                }
            }
        }
        if (!suppress) {
            if (out_count != i) out[out_count] = out[i];
            out_count++;
        }
    }
    return out_count;
}

// ---------------------------------------------------------------------------
// C interface — parameter setting
// ---------------------------------------------------------------------------

int wc_set_parameter(void *handle, const char *kind, int value) {
    if (!handle || !kind) return -1;
    WcSessionHandle *h = (__bridge WcSessionHandle *)handle;
    if (![h uvcAvailable]) return -1;

    // Look up descriptor.
    const ControlDesc *d = NULL;
    for (int i = 0; i < kControlCount; i++) {
        if (strcmp(kControls[i].kind, kind) == 0) { d = &kControls[i]; break; }
    }
    if (!d) return -1;

    // CMIO cooperation: switch the kernel driver's auto/manual state before the UVC write.
    // Range controls always force manual (0); auto toggles mirror the requested value.
    CMIOObjectID cmioID = (CMIOObjectID)[h cmioDeviceID];
    if (cmioID != kCMIOObjectUnknown && d->cmio_auto_class != 0) {
        UInt32 autoVal = (d->presentation == CTRL_RANGE) ? 0 : (value ? 1 : 0);
        cmio_set_auto_manual(cmioID, d->cmio_auto_class, autoVal);
    }

    int ret = [h uvcWriteKind:kind value:(int32_t)value];

    // AVFoundation continuously re-applies its own auto modes; sync it after a successful write
    // so it stops fighting our UVC state.
    if (ret == 0 && d->avf_sync != AVF_NONE) {
        AVCaptureDevice *dev = h.device;
        BOOL isAuto = (value != 0);
        if ([dev lockForConfiguration:nil]) {
            switch (d->avf_sync) {
                case AVF_FOCUS: {
                    AVCaptureFocusMode mode = isAuto ? AVCaptureFocusModeContinuousAutoFocus
                                                     : AVCaptureFocusModeLocked;
                    if ([dev isFocusModeSupported:mode]) dev.focusMode = mode;
                    break;
                }
                case AVF_WHITE_BALANCE: {
                    AVCaptureWhiteBalanceMode mode = isAuto
                        ? AVCaptureWhiteBalanceModeContinuousAutoWhiteBalance
                        : AVCaptureWhiteBalanceModeLocked;
                    if ([dev isWhiteBalanceModeSupported:mode]) dev.whiteBalanceMode = mode;
                    break;
                }
                case AVF_EXPOSURE: {
                    AVCaptureExposureMode mode = isAuto ? AVCaptureExposureModeContinuousAutoExposure
                                                        : AVCaptureExposureModeLocked;
                    if ([dev isExposureModeSupported:mode]) dev.exposureMode = mode;
                    break;
                }
                case AVF_NONE:
                    break;
            }
            [dev unlockForConfiguration];
        }
    }

    return ret;
}
