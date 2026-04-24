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
// UVC direct-write layer (IOKit)
// ---------------------------------------------------------------------------

#define UVC_SET_CUR            0x01
#define UVC_CS_INTERFACE       0x24
#define UVC_VC_INPUT_TERMINAL  0x02
#define UVC_VC_PROCESSING_UNIT 0x05
#define UVC_ITT_CAMERA         0x0201

typedef struct { const char *kind; uint8_t selector; BOOL isPU; uint8_t size; } UVCEntry;

// Processing Unit (PU) and Camera Terminal (CT) controls with UVC selectors and data sizes.
static const UVCEntry kUVCControls[] = {
    // Processing Unit
    { "backlight_compensation",    0x01, YES, 2 },
    { "brightness",                0x02, YES, 2 },
    { "contrast",                  0x03, YES, 2 },
    { "gain",                      0x04, YES, 2 },
    { "power_line_frequency",      0x05, YES, 1 }, // enum: 0=disabled,1=50Hz,2=60Hz
    { "hue",                       0x06, YES, 2 },
    { "saturation",                0x07, YES, 2 },
    { "sharpness",                 0x08, YES, 2 },
    { "gamma",                     0x09, YES, 2 },
    { "white_balance_temperature", 0x0A, YES, 2 },
    { "white_balance_auto",        0x0B, YES, 1 },
    { "color_enable",              0x0C, YES, 1 },
    { "hue_auto",                  0x0F, YES, 1 }, // UVC 1.5
    // Camera Terminal
    { "exposure_auto",             0x02, NO,  1 }, // CT AE mode: 1=manual, 8=aperture priority
    { "exposure_time_absolute",    0x04, NO,  4 }, // CT, 100µs units
    { "focus_absolute",            0x06, NO,  2 },
    { "focus_auto",                0x08, NO,  1 },
    { "iris_absolute",             0x09, NO,  2 },
    { "zoom_absolute",             0x0B, NO,  2 },
    { "pan_absolute",              0x0D, NO,  4 }, // signed 32-bit, arcseconds
    { "tilt_absolute",             0x0E, NO,  4 }, // signed 32-bit, arcseconds
};
static const int kUVCControlCount = (int)(sizeof(kUVCControls) / sizeof(kUVCControls[0]));

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
// Uses IOUSBInterfaceInterface::ControlRequest (VVUVCKit approach) instead of
// IOUSBDeviceInterface::DeviceRequest. The kernel allows class requests through the
// interface object even when it has the device open, whereas DeviceRequest is blocked
// for some CT controls (e.g. CT_EXPOSURE_TIME_ABSOLUTE) when AVFoundation runs AE.
static IOUSBInterfaceInterface190 **uvc_open_vc_interface(NSString *uniqueID,
                                                           uint8_t *outVCIf,
                                                           uint8_t *outPU,
                                                           uint8_t *outCT) {
    *outVCIf = 0xFF; *outPU = 0; *outCT = 0;
    NSLog(@"[uvc] open uniqueID=%@", uniqueID);

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

    // 6. Open the interface. kIOReturnExclusiveAccess is expected (AVFoundation holds it)
    //    and is acceptable: ControlRequest for class requests still works.
    IOReturn openKr = (*intf)->USBInterfaceOpen(intf);
    NSLog(@"[uvc] USBInterfaceOpen kr=0x%X", openKr);
    if (openKr != kIOReturnSuccess && openKr != kIOReturnExclusiveAccess) {
        (*intf)->Release(intf);
        return NULL;
    }
    return intf;
}

// Send a UVC SET_CUR request via the VideoControl interface object.
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

// Generic UVC GET request (GET_CUR=0x81, GET_MIN=0x82, GET_MAX=0x83, GET_RES=0x84).
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

static int uvc_get_cur(IOUSBInterfaceInterface190 **intf, uint8_t unitID, uint8_t selector,
                        uint8_t ifNum, void *data, uint16_t len) {
    return uvc_get_req(intf, 0x81, unitID, selector, ifNum, data, len);
}

// Read a 1/2/4-byte UVC value and return it as a signed int32_t (little-endian).
// 1-byte values are treated as unsigned; 2-byte as signed int16; 4-byte as signed int32.
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
- (int)uvcReadCTSelector:(uint8_t)selector intoBytes:(void *)buf len:(uint16_t)len;
- (int)uvcGetPUSelector:(uint8_t)selector request:(uint8_t)req intoBytes:(void *)buf len:(uint16_t)len;
- (int)uvcGetSelector:(uint8_t)selector request:(uint8_t)req isPU:(BOOL)isPU out:(int32_t *)out size:(uint8_t)size;
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

- (int)uvcReadCTSelector:(uint8_t)selector intoBytes:(void *)buf len:(uint16_t)len {
    if (!_uvcIF || !_uvcCT) return -1;
    return uvc_get_cur(_uvcIF, _uvcCT, selector, _uvcVCIf, buf, len);
}

- (int)uvcGetPUSelector:(uint8_t)selector request:(uint8_t)req intoBytes:(void *)buf len:(uint16_t)len {
    if (!_uvcIF || !_uvcPU) return -1;
    return uvc_get_req(_uvcIF, req, _uvcPU, selector, _uvcVCIf, buf, len);
}

- (int)uvcGetSelector:(uint8_t)selector request:(uint8_t)req isPU:(BOOL)isPU out:(int32_t *)out size:(uint8_t)size {
    if (!_uvcIF) return -1;
    uint8_t unitID = isPU ? _uvcPU : _uvcCT;
    if (!unitID) return -1;
    return uvc_read_int(_uvcIF, req, unitID, selector, _uvcVCIf, size, out);
}

- (int)uvcWriteKind:(const char *)kind value:(int32_t)value {
    const UVCEntry *entry = NULL;
    for (int i = 0; i < kUVCControlCount; i++) {
        if (strcmp(kUVCControls[i].kind, kind) == 0) { entry = &kUVCControls[i]; break; }
    }
    if (!entry) return -1;

    uint8_t unitID = entry->isPU ? _uvcPU : _uvcCT;
    if (!unitID) return -1;

    // Map logical auto values to UVC AE mode bitmask.
    // CMIO/UI uses 0=manual, 1=auto; UVC CT uses 1=manual, 8=aperture priority.
    int32_t uvcVal = value;
    if (strcmp(kind, "exposure_auto") == 0)
        uvcVal = (value == 0) ? 1 : 8;

    // exposure_time_absolute requires the camera to be in manual AE mode.
    // Re-assert manual AE mode and verify via GET_CUR before writing the time value.
    if (strcmp(kind, "exposure_time_absolute") == 0 && _uvcCT) {
        uint8_t aeManual = 1;
        uvc_set_cur(_uvcIF, _uvcCT, 0x02, _uvcVCIf, &aeManual, 1);
        uint8_t readBack = 0xFF;
        uvc_get_cur(_uvcIF, _uvcCT, 0x02, _uvcVCIf, &readBack, 1);
        NSLog(@"[uvc] AE mode after manual assert: camera reports 0x%02X (want 0x01)", readBack);
    }

    uint8_t buf[4] = {0};
    switch (entry->size) {
        case 1: buf[0] = (uint8_t)uvcVal; break;
        case 2: { uint16_t v = (uint16_t)(int16_t)uvcVal; memcpy(buf, &v, 2); break; }
        case 4: { uint32_t v = (uint32_t)uvcVal;           memcpy(buf, &v, 4); break; }
        default: return -1;
    }
    NSLog(@"[uvc] write kind=%s value=%d bytes=[%02X %02X %02X %02X]",
          kind, (int)uvcVal, buf[0], buf[1], buf[2], buf[3]);
    return uvc_set_cur(_uvcIF, unitID, entry->selector, _uvcVCIf, buf, entry->size);
}
@end

// ---------------------------------------------------------------------------
// CMIOHardware helpers (read-only — used for parameter enumeration)
// ---------------------------------------------------------------------------

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
};
static const int kCmioKindCount = (int)(sizeof(kCmioKinds) / sizeof(kCmioKinds[0]));

static const char *cmio_kind_for_class(uint32_t classID) {
    for (int i = 0; i < kCmioKindCount; i++)
        if (kCmioKinds[i].classID == classID) return kCmioKinds[i].kind;
    return NULL;
}

typedef struct { const char *range_kind; const char *auto_kind; } AutoKindEntry;
static const AutoKindEntry kAutoKinds[] = {
    { "exposure_time_absolute",    "exposure_auto"      },
    { "white_balance_temperature", "white_balance_auto" },
    { "focus_absolute",            "focus_auto"         },
};
static const int kAutoKindCount = (int)(sizeof(kAutoKinds) / sizeof(kAutoKinds[0]));

static const char *cmio_auto_kind_for_range_kind(const char *rangeKind) {
    for (int i = 0; i < kAutoKindCount; i++)
        if (strcmp(kAutoKinds[i].range_kind, rangeKind) == 0)
            return kAutoKinds[i].auto_kind;
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

// Try to set kCMIOFeatureControlPropertyAutomaticManual for a given control class.
// Returns YES if the property was settable and the write succeeded.
static BOOL cmio_set_auto_manual(CMIOObjectID deviceID, uint32_t controlClassID, UInt32 value) {
    NSArray<NSNumber *> *controls = cmio_collect_controls(deviceID);
    for (NSNumber *objNum in controls) {
        CMIOObjectID ctrl = (CMIOObjectID)objNum.unsignedIntValue;
        if (cmio_get_class(ctrl) != controlClassID) continue;
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
    return NO;
}

static Float32 cmio_scale_for_kind(__unused const char *kind) {
    return 1.0f;
}

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

    Float32 scale = cmio_scale_for_kind(kind);
    WcParamDesc *p = &out[(*count)++];
    memset(p, 0, sizeof(*p));
    strlcpy(p->kind, kind, WC_MAX_KIND);
    p->current = (int)roundf(cur * scale);
    p->is_range = 1;
    p->min      = (int)roundf((Float32)range.mMinimum * scale);
    p->max      = (int)roundf((Float32)range.mMaximum * scale);
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

    // Open UVC VideoControl interface for direct parameter writes via ControlRequest.
    uint8_t vcIf = 0, pu = 0, ct = 0;
    IOUSBInterfaceInterface190 **uvcIF = uvc_open_vc_interface(device.uniqueID, &vcIf, &pu, &ct);
    if (uvcIF) {
        [handle setUvcInterface:uvcIF vcInterface:vcIf pu:pu ct:ct];
    }

    return (__bridge_retained void *)handle;
}

void wc_close_session(void *handle) {
    if (!handle) return;
    WcSessionHandle *h = (__bridge_transfer WcSessionHandle *)handle;
    [h.session stopRunning];
    // WcSessionHandle dealloc closes the UVC device.
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
// C interface — parameter enumeration (reads via CMIO)
// ---------------------------------------------------------------------------

int wc_get_parameters(void *handle, WcParamDesc *out, int capacity) {
    if (!handle || !out || capacity <= 0) return 0;
    WcSessionHandle *h = (__bridge WcSessionHandle *)handle;
    AVCaptureDevice *dev = h.device;
    if (!dev) return 0;

    int count = 0;
    CMIOObjectID cmioID = (CMIOObjectID)[h cmioDeviceID];
    if (cmioID != kCMIOObjectUnknown) {
        NSArray<NSNumber *> *controls = cmio_collect_controls(cmioID);
        for (NSNumber *objNum in controls) {
            CMIOObjectID ctrl = (CMIOObjectID)objNum.unsignedIntValue;
            const char *kind = cmio_kind_for_class(cmio_get_class(ctrl));
            if (!kind) continue;
            const char *autoKind = cmio_auto_kind_for_range_kind(kind);
            if (autoKind) push_cmio_auto_manual(out, &count, capacity, ctrl, autoKind);
            push_cmio_range(out, &count, capacity, ctrl, kind);
        }
    }

    // Some drivers (e.g. Logitech on macOS) expose the absolute controls via CMIO but not
    // the AutomaticManual toggle, or the toggle reflects AVFoundation's state rather than
    // the true UVC hardware state. Fall back to UVC GET_CUR for auto toggles on CT controls.
    if ([h uvcHasCT]) {
        // focus_auto
        if (count < capacity) {
            BOOL hasFocusAbsolute = NO, hasFocusAuto = NO;
            for (int i = 0; i < count; i++) {
                if (strcmp(out[i].kind, "focus_absolute") == 0) hasFocusAbsolute = YES;
                if (strcmp(out[i].kind, "focus_auto")     == 0) hasFocusAuto     = YES;
            }
            if (hasFocusAbsolute && !hasFocusAuto) {
                uint8_t v = 0;
                if ([h uvcReadCTSelector:0x08 intoBytes:&v len:1] == 0) {
                    WcParamDesc *p = &out[count++];
                    memset(p, 0, sizeof(*p));
                    strlcpy(p->kind, "focus_auto", WC_MAX_KIND);
                    p->current = (int)v;
                    push_option(p, 0, "Manual");
                    push_option(p, 1, "Auto");
                }
            }
        }
        // exposure_auto: always read via UVC GET_CUR so the state reflects actual hardware,
        // not AVFoundation's view (which AVFoundation may have overridden).
        if (count < capacity) {
            BOOL hasExposureAbsolute = NO, hasExposureAuto = NO;
            for (int i = 0; i < count; i++) {
                if (strcmp(out[i].kind, "exposure_time_absolute") == 0) hasExposureAbsolute = YES;
                if (strcmp(out[i].kind, "exposure_auto")          == 0) hasExposureAuto     = YES;
            }
            if (hasExposureAbsolute) {
                uint8_t aeMode = 0;
                if ([h uvcReadCTSelector:0x02 intoBytes:&aeMode len:1] == 0) {
                    // UVC AE mode: 1=manual→0, anything else→1 (auto)
                    int logicalAuto = (aeMode == 1) ? 0 : 1;
                    if (hasExposureAuto) {
                        // Update existing CMIO-emitted entry with the real hardware value.
                        for (int i = 0; i < count; i++) {
                            if (strcmp(out[i].kind, "exposure_auto") == 0) {
                                out[i].current = logicalAuto;
                                break;
                            }
                        }
                    } else {
                        WcParamDesc *p = &out[count++];
                        memset(p, 0, sizeof(*p));
                        strlcpy(p->kind, "exposure_auto", WC_MAX_KIND);
                        p->current = logicalAuto;
                        push_option(p, 0, "Manual");
                        push_option(p, 1, "Auto");
                    }
                }
            }
        }

        // white_balance fallback: CMIO often doesn't expose the temperature range on UVC
        // cameras (especially when WB is in auto). Read CUR/MIN/MAX/RES via PU GET requests.
        if ([h uvcHasPU] && count < capacity) {
            BOOL hasWBTemp = NO, hasWBAuto = NO;
            for (int i = 0; i < count; i++) {
                if (strcmp(out[i].kind, "white_balance_temperature") == 0) hasWBTemp = YES;
                if (strcmp(out[i].kind, "white_balance_auto")        == 0) hasWBAuto = YES;
            }
            if (!hasWBTemp) {
                uint16_t cur = 0, min = 0, max = 0, res = 0;
                if ([h uvcGetPUSelector:0x0A request:0x81 intoBytes:&cur len:2] == 0 &&
                    [h uvcGetPUSelector:0x0A request:0x82 intoBytes:&min len:2] == 0 &&
                    [h uvcGetPUSelector:0x0A request:0x83 intoBytes:&max len:2] == 0 &&
                    min < max) {
                    [h uvcGetPUSelector:0x0A request:0x84 intoBytes:&res len:2];
                    WcParamDesc *p = &out[count++];
                    memset(p, 0, sizeof(*p));
                    strlcpy(p->kind, "white_balance_temperature", WC_MAX_KIND);
                    p->current  = (int)(int16_t)OSSwapLittleToHostInt16(cur);
                    p->is_range = 1;
                    p->min      = (int)(int16_t)OSSwapLittleToHostInt16(min);
                    p->max      = (int)(int16_t)OSSwapLittleToHostInt16(max);
                    p->step     = (res > 0) ? (int)(int16_t)OSSwapLittleToHostInt16(res) : 1;
                }
            }
            if (!hasWBAuto && count < capacity) {
                uint8_t v = 0;
                if ([h uvcGetPUSelector:0x0B request:0x81 intoBytes:&v len:1] == 0) {
                    WcParamDesc *p = &out[count++];
                    memset(p, 0, sizeof(*p));
                    strlcpy(p->kind, "white_balance_auto", WC_MAX_KIND);
                    p->current = (int)v;
                    push_option(p, 0, "Manual");
                    push_option(p, 1, "Auto");
                }
            }
        }
    }

    // Generic UVC fallback: probe any remaining controls not yet emitted by CMIO/specific blocks.
    if ([h uvcAvailable]) {
        for (int i = 0; i < kUVCControlCount && count < capacity; i++) {
            const UVCEntry *e = &kUVCControls[i];

            // Skip if already in output.
            BOOL alreadyPresent = NO;
            for (int j = 0; j < count; j++) {
                if (strcmp(out[j].kind, e->kind) == 0) { alreadyPresent = YES; break; }
            }
            if (alreadyPresent) continue;

            // Skip if required unit is not available for this camera.
            if (e->isPU && ![h uvcHasPU]) continue;
            if (!e->isPU && ![h uvcHasCT]) continue;

            // Probe GET_CUR — if this control doesn't exist on the camera, skip it.
            int32_t cur = 0;
            if ([h uvcGetSelector:e->selector request:0x81 isPU:e->isPU out:&cur size:e->size] != 0)
                continue;

            WcParamDesc *p = &out[count];
            memset(p, 0, sizeof(*p));
            strlcpy(p->kind, e->kind, WC_MAX_KIND);
            p->current = (int)cur;

            // power_line_frequency: fixed enum regardless of GET_MIN/MAX.
            if (strcmp(e->kind, "power_line_frequency") == 0) {
                push_option(p, 0, "Disabled");
                push_option(p, 1, "50 Hz");
                push_option(p, 2, "60 Hz");
                count++;
                continue;
            }

            // Pure on/off toggles with no range meaning.
            if (strcmp(e->kind, "color_enable") == 0) {
                push_option(p, 0, "Off");
                push_option(p, 1, "On");
                count++;
                continue;
            }
            if (strcmp(e->kind, "hue_auto") == 0) {
                push_option(p, 0, "Manual");
                push_option(p, 1, "Auto");
                count++;
                continue;
            }

            // For everything else try GET_MIN / GET_MAX; emit as range if valid.
            int32_t minVal = 0, maxVal = 0;
            if ([h uvcGetSelector:e->selector request:0x82 isPU:e->isPU out:&minVal size:e->size] == 0 &&
                [h uvcGetSelector:e->selector request:0x83 isPU:e->isPU out:&maxVal size:e->size] == 0 &&
                minVal < maxVal) {
                int32_t res = 1;
                [h uvcGetSelector:e->selector request:0x84 isPU:e->isPU out:&res size:e->size];
                p->is_range = 1;
                p->min      = (int)minVal;
                p->max      = (int)maxVal;
                p->step     = (res > 0) ? (int)res : 1;
                count++;
            }
            // If no valid range is available, discard (don't increment count).
        }
    }

    return count;
}

// ---------------------------------------------------------------------------
// C interface — parameter setting (writes via UVC/IOKit)
// ---------------------------------------------------------------------------

int wc_set_parameter(void *handle, const char *kind, int value) {
    if (!handle || !kind) return -1;
    WcSessionHandle *h = (__bridge WcSessionHandle *)handle;
    if (![h uvcAvailable]) return -1;

    // For exposure controls the kernel UVC driver manages AE state internally.
    // Cooperate via CMIO so the kernel driver switches mode before we send UVC commands.
    CMIOObjectID cmioID = (CMIOObjectID)[h cmioDeviceID];
    if (cmioID != kCMIOObjectUnknown) {
        if (strcmp(kind, "exposure_auto") == 0) {
            cmio_set_auto_manual(cmioID, kCMIOExposureControlClassID, value ? 1 : 0);
        } else if (strcmp(kind, "exposure_time_absolute") == 0) {
            cmio_set_auto_manual(cmioID, kCMIOExposureControlClassID, 0);
        } else if (strcmp(kind, "white_balance_auto") == 0) {
            cmio_set_auto_manual(cmioID, kCMIOTemperatureControlClassID, value ? 1 : 0);
        }
    }

    int ret = [h uvcWriteKind:kind value:(int32_t)value];

    // AVFoundation re-applies its auto modes continuously — sync mode so it stops
    // fighting our UVC writes for focus and white balance.
    if (ret == 0) {
        AVCaptureDevice *dev = h.device;
        if (strcmp(kind, "focus_auto") == 0 && [dev lockForConfiguration:nil]) {
            BOOL isAuto = (value != 0);
            AVCaptureFocusMode mode = isAuto ? AVCaptureFocusModeContinuousAutoFocus
                                             : AVCaptureFocusModeLocked;
            if ([dev isFocusModeSupported:mode]) dev.focusMode = mode;
            [dev unlockForConfiguration];
        } else if (strcmp(kind, "white_balance_auto") == 0 && [dev lockForConfiguration:nil]) {
            BOOL isAuto = (value != 0);
            AVCaptureWhiteBalanceMode mode = isAuto ? AVCaptureWhiteBalanceModeContinuousAutoWhiteBalance
                                                    : AVCaptureWhiteBalanceModeLocked;
            if ([dev isWhiteBalanceModeSupported:mode]) dev.whiteBalanceMode = mode;
            [dev unlockForConfiguration];
        } else if (strcmp(kind, "exposure_auto") == 0 && [dev lockForConfiguration:nil]) {
            // Lock AVFoundation's AE loop so it stops overriding exposure_time_absolute.
            BOOL isAuto = (value != 0);
            AVCaptureExposureMode mode = isAuto ? AVCaptureExposureModeContinuousAutoExposure
                                                : AVCaptureExposureModeLocked;
            if ([dev isExposureModeSupported:mode]) dev.exposureMode = mode;
            [dev unlockForConfiguration];
        }
    }

    return ret;
}
