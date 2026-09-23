#include "nx_key_bridge.h"

#include <IOKit/IOKitLib.h>
#include <IOKit/hidsystem/IOHIDLib.h>
#include <IOKit/hidsystem/event_status_driver.h>
#include <mach/mach.h>
#include <stdlib.h>
#include <string.h>

struct MousehopNxDriver {
  io_connect_t connection;
};

/*
 * The last argument of IOHIDPostEvent is an IOOptionBits options mask, not a
 * boolean. Passing this bit asks the window server to adopt the event's
 * `eventFlags` as the global modifier state, which is what makes a modifier
 * transition a physical-looking HID modifier press. Ordinary keys must pass
 * it clear so they inherit the state the flags-changed events established.
 * The reference implementation (Deskflow OSXKeyState.cpp) and the comparison
 * app that validated this path both pass the same value, 1.
 */
#define MOUSEHOP_NX_SET_GLOBAL_FLAGS ((IOOptionBits)1)

MousehopNxDriver *mousehop_nx_open(void)
{
  io_service_t service;
  io_connect_t connection = 0;
  kern_return_t result;
  MousehopNxDriver *driver;

  service = IOServiceGetMatchingService(kIOMainPortDefault,
                                        IOServiceMatching(kIOHIDSystemClass));
  if (!service) {
    return NULL;
  }

  result = IOServiceOpen(service, mach_task_self(), kIOHIDParamConnectType,
                         &connection);
  IOObjectRelease(service);
  if (result != KERN_SUCCESS || !connection) {
    return NULL;
  }

  driver = (MousehopNxDriver *)calloc(1, sizeof(MousehopNxDriver));
  if (!driver) {
    IOServiceClose(connection);
    return NULL;
  }
  driver->connection = connection;
  return driver;
}

void mousehop_nx_close(MousehopNxDriver *driver)
{
  if (!driver) {
    return;
  }
  if (driver->connection) {
    IOServiceClose(driver->connection);
  }
  free(driver);
}

int32_t mousehop_nx_flags_changed(MousehopNxDriver *driver,
                                  uint16_t key_code,
                                  uint32_t flags)
{
  NXEventData event;
  IOGPoint location;

  if (!driver) {
    return (int32_t)KERN_INVALID_ARGUMENT;
  }

  memset(&event, 0, sizeof(event));
  /* The key code must be set for every event type, including a modifier
   * transition: some input methods read the default zero as the `a` key. */
  event.key.keyCode = (unsigned short)key_code;
  location.x = 0;
  location.y = 0;

  /* The location is meaningless for key events; only the flags are adopted. */
  return (int32_t)IOHIDPostEvent(driver->connection, NX_FLAGSCHANGED, location,
                                 &event, kNXEventDataVersion, flags,
                                 MOUSEHOP_NX_SET_GLOBAL_FLAGS);
}

int32_t mousehop_nx_key(MousehopNxDriver *driver, uint16_t key_code, int32_t down)
{
  NXEventData event;
  IOGPoint location;

  if (!driver) {
    return (int32_t)KERN_INVALID_ARGUMENT;
  }

  memset(&event, 0, sizeof(event));
  event.key.keyCode = (unsigned short)key_code;
  location.x = 0;
  location.y = 0;

  /* No global flags and no cursor update: ordinary keys inherit the modifier
   * state the flags-changed events established. */
  return (int32_t)IOHIDPostEvent(driver->connection,
                                 down ? NX_KEYDOWN : NX_KEYUP, location, &event,
                                 kNXEventDataVersion, 0, 0);
}
