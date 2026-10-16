#import <AppKit/AppKit.h>
#import <WebKit/WebKit.h>
#import <dispatch/dispatch.h>
#include <stdatomic.h>

static NSString *POMChatURL;
static NSString *POMWindowURL;
static NSString *POMIconPath;
static NSString *POMApplicationName;
static NSStatusItem *POMStatusItem;
static NSPopover *POMPopover;
static NSWindow *POMWindow;
static WKWebView *POMPopoverWebView;
static WKWebView *POMWindowWebView;
static id POMNavigationDelegateInstance;
static id POMScriptHandlerInstance;
static id POMStatusControllerInstance;
static id POMApplicationDelegateInstance;
static atomic_bool POMStopRequested = false;
static atomic_bool POMUserQuitRequested = false;

static const CGFloat POMStatusIconPoints = 18.0;

// Measurements of the panel and the window. The one that chooses them is `macos_status.rs`, which
// hands them over at startup; here they only have an initial value for the case where the
// bridge is called with zeros.
static CGFloat POMPopoverWidth = 390.0;
static CGFloat POMPopoverHeight = 844.0;
static CGFloat POMWindowWidth = 1080.0;
static CGFloat POMWindowHeight = 760.0;

static NSString *POMLocalized(NSString *key, NSString *fallback) {
    return [[NSBundle mainBundle] localizedStringForKey:key value:fallback table:nil];
}

static void POMOpenWindow(void);
static void POMWaitForGateway(WKWebView *webView);
static void POMShowUnavailable(WKWebView *webView);
static WKWebView *POMMakeWebView(NSRect frame);
static NSImage *POMStatusImage(void);
static void POMInstallApplicationMenu(void);

@interface POMNavigationDelegate : NSObject <WKNavigationDelegate>
@end

@interface POMScriptHandler : NSObject <WKScriptMessageHandler>
@end

@interface POMStatusController : NSObject <NSWindowDelegate>
- (void)togglePopover:(id)sender;
- (void)openWindow:(id)sender;
- (void)reload:(id)sender;
- (void)reloadAll:(id)sender;
@end

@interface POMApplicationDelegate : NSObject <NSApplicationDelegate>
@end

/// Height of the panel that fits on the screen where the menu bar is.
///
/// The requested measure is that of a phone, 844 points. An older 13" MacBook Pro
/// has 800 points of height in total, and the panel would stick out through the bottom
/// edge. Here it shrinks to what is left below the menu bar, with a
/// slack so it does not touch the bottom; on a large screen the requested measure passes
/// through intact.
static CGFloat POMFittedPopoverHeight(void) {
    NSScreen *screen = POMStatusItem.button.window.screen ?: NSScreen.mainScreen;
    if (!screen) {
        return POMPopoverHeight;
    }
    const CGFloat margin = 24.0;
    CGFloat available = NSHeight(screen.visibleFrame) - margin;
    return MAX(320.0, MIN(POMPopoverHeight, available));
}

static void POMOpenPopover(void) {
    CGFloat height = POMFittedPopoverHeight();

    if (!POMPopover) {
        POMPopover = [[NSPopover alloc] init];
        POMPopover.behavior = NSPopoverBehaviorTransient;
        POMPopover.animates = YES;

        NSViewController *viewController = [[NSViewController alloc] init];
        POMPopoverWebView = POMMakeWebView(NSMakeRect(0, 0, POMPopoverWidth, height));
        POMWaitForGateway(POMPopoverWebView);
        viewController.view = POMPopoverWebView;
        POMPopover.contentViewController = viewController;
    }

    // Recomputed on every opening: the laptop may have been connected to a
    // monitor - or disconnected from one - since the last time.
    POMPopover.contentSize = NSMakeSize(POMPopoverWidth, height);

    NSButton *button = POMStatusItem.button;
    [POMPopover showRelativeToRect:button.bounds
                            ofView:button
                     preferredEdge:NSRectEdgeMaxY];
}

static void POMTogglePopover(void) {
    if (POMPopover.isShown) {
        [POMPopover close];
    } else {
        POMOpenPopover();
    }
}

static void POMOpenWindow(void) {
    [POMPopover close];

    if (!POMWindow) {
        POMWindow = [[NSWindow alloc]
            initWithContentRect:NSMakeRect(0, 0, POMWindowWidth, POMWindowHeight)
                      styleMask:(NSWindowStyleMaskTitled |
                                 NSWindowStyleMaskClosable |
                                 NSWindowStyleMaskResizable |
                                 NSWindowStyleMaskMiniaturizable)
                        backing:NSBackingStoreBuffered
                          defer:NO];
        POMWindow.title = POMApplicationName;
        POMWindow.minSize = NSMakeSize(640, 480);
        // The green button, and View > Enter Full Screen, make the window full screen
        // like any native macOS app.
        POMWindow.collectionBehavior |= NSWindowCollectionBehaviorFullScreenPrimary;
        // Closing hides the window; the app keeps running in the menu bar.
        POMWindow.releasedWhenClosed = NO;
        POMWindow.delegate = POMStatusControllerInstance;
        POMWindowWebView = POMMakeWebView(NSMakeRect(0, 0, POMWindowWidth, POMWindowHeight));
        POMWaitForGateway(POMWindowWebView);
        POMWindow.contentView = POMWindowWebView;
        [POMWindow center];
    }

    [POMWindow makeKeyAndOrderFront:nil];
    [NSApp activateIgnoringOtherApps:YES];
}

/// The status item's context menu: open the larger window, or quit. It is the
/// one place the user can always quit from, window open or not.
static void POMShowStatusMenu(void) {
    [POMPopover close];
    NSMenu *menu = [[NSMenu alloc] initWithTitle:@"POM"];
    NSMenuItem *open = [[NSMenuItem alloc]
        initWithTitle:POMLocalized(@"status.openWindow", @"Open in a larger window")
               action:@selector(openWindow:)
        keyEquivalent:@""];
    open.target = POMStatusControllerInstance;
    [menu addItem:open];
    NSMenuItem *reload = [[NSMenuItem alloc]
        initWithTitle:POMLocalized(@"status.reload", @"Reload")
               action:@selector(reloadAll:)
        keyEquivalent:@""];
    reload.target = POMStatusControllerInstance;
    [menu addItem:reload];
    [menu addItem:[NSMenuItem separatorItem]];
    [menu addItem:[[NSMenuItem alloc]
        initWithTitle:POMLocalized(@"status.quit", @"Quit POM")
               action:@selector(terminate:)
        keyEquivalent:@"q"]];
    NSEvent *event = NSApp.currentEvent;
    NSButton *button = POMStatusItem.button;
    if (event) {
        [NSMenu popUpContextMenu:menu withEvent:event forView:button];
    } else {
        [menu popUpMenuPositioningItem:nil
                            atLocation:NSMakePoint(0, NSHeight(button.bounds) + 4)
                                inView:button];
    }
}


static NSString *POMUnavailableHTML(void) {
    NSString *title = POMLocalized(
        @"status.gatewayUnavailable.title",
        @"POM could not reach the gateway");
    NSString *detail = POMLocalized(
        @"status.gatewayUnavailable.detail",
        @"The local gateway has not responded yet. Start the node and try again.");
    NSString *retry = POMLocalized(
        @"status.gatewayUnavailable.retry",
        @"Try again");

    return [NSString stringWithFormat:
        @"<!doctype html><html><head><meta charset='utf-8'>"
         "<meta name='viewport' content='width=device-width,initial-scale=1'>"
         "<style>"
         "*{box-sizing:border-box}body{margin:0;min-height:100vh;display:grid;"
         "place-items:center;background:#090a10;color:#e8eaf2;font-family:-apple-system,"
         "BlinkMacSystemFont,'Helvetica Neue',sans-serif;padding:32px;text-align:center}"
         "main{max-width:320px}h1{font-size:22px;line-height:1.2;margin:0 0 12px}"
         "p{font-size:14px;line-height:1.55;color:#8a90a6;margin:0 0 24px}"
         "button{border:1px solid #8a7cff;background:#8a7cff;color:#090a10;"
         "font:600 14px -apple-system,BlinkMacSystemFont,'Helvetica Neue',sans-serif;"
         "border-radius:10px;padding:10px 16px;cursor:pointer}"
         "button:hover{filter:brightness(1.1)}</style></head><body><main>"
         "<h1>%@</h1><p>%@</p>"
         "<button onclick=\"window.webkit.messageHandlers.pomRetryGateway.postMessage('retry')\">%@</button>"
         "</main></body></html>",
        title,
        detail,
        retry];
}

/// The web views that are waiting for the gateway to answer.
static NSMutableSet *POMWaitingWebViews;
/// The web views showing one of the native pages (loading or unavailable) instead
/// of the interface. Their URL is the gateway's base, so the URL cannot tell.
static NSMutableSet *POMPlaceholderWebViews;

static void POMMarkPlaceholder(WKWebView *webView, BOOL placeholder) {
    if (!POMPlaceholderWebViews) {
        POMPlaceholderWebViews = [NSMutableSet set];
    }
    if (placeholder) {
        [POMPlaceholderWebViews addObject:webView];
    } else {
        [POMPlaceholderWebViews removeObject:webView];
    }
}

static void POMShowUnavailable(WKWebView *webView) {
    POMMarkPlaceholder(webView, YES);
    [webView loadHTMLString:POMUnavailableHTML() baseURL:[NSURL URLWithString:POMChatURL]];
}
/// 120 probes, half a second apart: a minute for the node to start.
static const int POMGatewayProbeLimit = 120;

static NSString *POMLoadingHTML(void) {
    NSString *message = POMLocalized(@"status.gatewayStarting", @"Starting POM…");
    return [NSString stringWithFormat:
        @"<!doctype html><html><head><meta charset='utf-8'>"
         "<meta name='viewport' content='width=device-width,initial-scale=1'>"
         "<style>"
         "*{box-sizing:border-box}body{margin:0;min-height:100vh;display:grid;"
         "place-items:center;background:#090a10;color:#8a90a6;font-family:-apple-system,"
         "BlinkMacSystemFont,'Helvetica Neue',sans-serif;text-align:center}"
         ".spin{width:28px;height:28px;margin:0 auto 16px;border-radius:50%%;"
         "border:3px solid #2a2d3d;border-top-color:#8a7cff;animation:spin .9s linear infinite}"
         "@keyframes spin{to{transform:rotate(360deg)}}p{font-size:14px;margin:0}"
         "</style></head><body><main><div class='spin'></div><p>%@</p></main></body></html>",
        message];
}

static NSString *POMAddressOf(WKWebView *webView) {
    return webView == POMWindowWebView ? POMWindowURL : POMChatURL;
}

static void POMProbeGateway(WKWebView *webView, int attempt);

static void POMScheduleGatewayProbe(WKWebView *webView, int attempt) {
    dispatch_after(dispatch_time(DISPATCH_TIME_NOW, (int64_t)(0.5 * NSEC_PER_SEC)),
                   dispatch_get_main_queue(), ^{
        POMProbeGateway(webView, attempt);
    });
}

/// Asks the node for the page. Any HTTP answer below 500 means the gateway is up;
/// a refused connection or a timeout means it is still starting.
static void POMProbeGateway(WKWebView *webView, int attempt) {
    if (![POMWaitingWebViews containsObject:webView]) {
        return;
    }
    NSURL *url = [NSURL URLWithString:POMAddressOf(webView)];
    if (!url) {
        [POMWaitingWebViews removeObject:webView];
        POMShowUnavailable(webView);
        return;
    }
    NSMutableURLRequest *request = [NSMutableURLRequest requestWithURL:url];
    request.timeoutInterval = 2.0;
    [[NSURLSession.sharedSession dataTaskWithRequest:request
                                   completionHandler:^(NSData *data, NSURLResponse *response,
                                                       NSError *error) {
        (void)data;
        NSInteger status = [response isKindOfClass:NSHTTPURLResponse.class]
            ? ((NSHTTPURLResponse *)response).statusCode
            : 0;
        BOOL up = error == nil && status > 0 && status < 500;
        dispatch_async(dispatch_get_main_queue(), ^{
            if (![POMWaitingWebViews containsObject:webView]) {
                return;
            }
            if (up) {
                [POMWaitingWebViews removeObject:webView];
                POMMarkPlaceholder(webView, NO);
                [webView loadRequest:[NSURLRequest requestWithURL:url]];
            } else if (attempt + 1 >= POMGatewayProbeLimit) {
                [POMWaitingWebViews removeObject:webView];
                POMShowUnavailable(webView);
            } else {
                POMScheduleGatewayProbe(webView, attempt + 1);
            }
        });
    }] resume];
}

/// Shows the loading screen and loads the page once the gateway answers. Used at
/// launch, when the page fails, and by the "Try again" button.
static void POMWaitForGateway(WKWebView *webView) {
    if (!POMWaitingWebViews) {
        POMWaitingWebViews = [NSMutableSet set];
    }
    if ([POMWaitingWebViews containsObject:webView]) {
        return;
    }
    [POMWaitingWebViews addObject:webView];
    POMMarkPlaceholder(webView, YES);
    NSURL *base = [NSURL URLWithString:POMAddressOf(webView)];
    [webView loadHTMLString:POMLoadingHTML() baseURL:base];
    POMScheduleGatewayProbe(webView, 0);
}

/// The browser's reload, which a WebKit window lacks: View > Reload (Cmd-R) and the
/// status item's menu. On the interface it reloads the current screen, fetching its
/// code again - the way out of a screen that stopped working. On a native page it
/// goes back to waiting for the gateway, since reloading that HTML would only show
/// it again.
static void POMReload(WKWebView *webView) {
    if (!webView || [POMWaitingWebViews containsObject:webView]) {
        return;
    }
    if ([POMPlaceholderWebViews containsObject:webView] || !webView.URL) {
        POMWaitForGateway(webView);
    } else {
        [webView reloadFromOrigin];
    }
}

/// The web view the user is looking at: the open panel, else the window.
static WKWebView *POMFrontWebView(void) {
    if (POMPopover.isShown) {
        return POMPopoverWebView;
    }
    return POMWindowWebView;
}

/// Builds the bar item image with the three resolutions of the template.
///
/// `POMIconPath` points at the 1x art; the companions `@2x` and `@3x` sit beside
/// it. Each representation enters with `size` forced to 18 x 18 **points**,
/// and that is what makes AppKit treat them as the same image at different
/// scales instead of three images of different sizes - without that, a
/// 54 px art would be drawn as a 54 point glyph.
///
/// The image is marked as a template: AppKit discards the color and paints the alpha
/// with the color of the bar, which settles the light theme and the dark one at once,
/// including when the user switches theme with the app already open.
static NSImage *POMStatusImage(void) {
    NSImage *image = [[NSImage alloc] initWithSize:NSMakeSize(POMStatusIconPoints,
                                                              POMStatusIconPoints)];
    NSString *directory = POMIconPath.stringByDeletingLastPathComponent;
    NSString *name = POMIconPath.lastPathComponent.stringByDeletingPathExtension;
    NSString *extension = POMIconPath.pathExtension;

    BOOL any = NO;
    for (NSString *suffix in @[ @"", @"@2x", @"@3x" ]) {
        NSString *path = [directory stringByAppendingPathComponent:
            [NSString stringWithFormat:@"%@%@.%@", name, suffix, extension]];
        NSImageRep *rep = [NSImageRep imageRepWithContentsOfFile:path];
        if (!rep) {
            continue;
        }
        rep.size = NSMakeSize(POMStatusIconPoints, POMStatusIconPoints);
        [image addRepresentation:rep];
        any = YES;
    }

    if (!any) {
        return nil;
    }
    image.template = YES;
    return image;
}

static WKWebView *POMMakeWebView(NSRect frame) {
    WKWebViewConfiguration *configuration = [[WKWebViewConfiguration alloc] init];
    configuration.websiteDataStore = [WKWebsiteDataStore defaultDataStore];
    [configuration.userContentController addScriptMessageHandler:POMScriptHandlerInstance
                                                             name:@"pomOpenWindow"];
    [configuration.userContentController addScriptMessageHandler:POMScriptHandlerInstance
                                                             name:@"pomRetryGateway"];
    [configuration.userContentController addScriptMessageHandler:POMScriptHandlerInstance
                                                             name:@"pomQuit"];

    WKWebView *webView = [[WKWebView alloc] initWithFrame:frame configuration:configuration];
    webView.navigationDelegate = POMNavigationDelegateInstance;
    webView.autoresizingMask = NSViewWidthSizable | NSViewHeightSizable;
    return webView;
}

@implementation POMNavigationDelegate

- (void)webView:(WKWebView *)webView
    didFailProvisionalNavigation:(WKNavigation *)navigation
        withError:(NSError *)error {
    (void)navigation;
    (void)error;
    POMShowUnavailable(webView);
}

@end

@implementation POMScriptHandler

- (void)userContentController:(WKUserContentController *)userContentController
      didReceiveScriptMessage:(WKScriptMessage *)message {
    (void)userContentController;
    if ([message.name isEqualToString:@"pomOpenWindow"]) {
        POMOpenWindow();
    } else if ([message.name isEqualToString:@"pomQuit"]) {
        [NSApp terminate:nil];
    } else if ([message.name isEqualToString:@"pomRetryGateway"] &&
               [message.webView isKindOfClass:[WKWebView class]]) {
        POMWaitForGateway((WKWebView *)message.webView);
    }
}

@end

@implementation POMStatusController

- (void)togglePopover:(id)sender {
    (void)sender;
    // Right click, or Control-click, opens the menu with "Quit POM"; a plain
    // click keeps opening the chat.
    NSEvent *event = NSApp.currentEvent;
    if (event.type == NSEventTypeRightMouseUp ||
        (event.modifierFlags & NSEventModifierFlagControl)) {
        POMShowStatusMenu();
        return;
    }
    POMTogglePopover();
}

- (void)openWindow:(id)sender {
    (void)sender;
    POMOpenWindow();
}

- (void)reload:(id)sender {
    (void)sender;
    POMReload(POMFrontWebView());
}

- (void)reloadAll:(id)sender {
    (void)sender;
    // The status item's menu closes the popover, so there is no front view to
    // pick: both are reloaded, and whichever the user opens next is fresh.
    POMReload(POMWindowWebView);
    POMReload(POMPopoverWebView);
}

- (void)windowWillClose:(NSNotification *)notification {
    if (notification.object == POMWindow) {
    }
}

@end

@implementation POMApplicationDelegate

- (NSApplicationTerminateReply)applicationShouldTerminate:(NSApplication *)sender {
    (void)sender;
    // The node service runs on a separate Tokio thread and has no cooperative
    // shutdown channel. Mark a user-initiated Quit so the Rust side can end
    // the process after AppKit returns from its event loop instead of joining
    // a service that is still serving forever.
    atomic_store(&POMUserQuitRequested, true);
    return NSTerminateNow;
}

- (void)applicationDidFinishLaunching:(NSNotification *)notification {
    (void)notification;
    // A regular app: Dock icon, menu bar and full screen, as any native macOS app. The
    // status item stays in the menu bar while the process runs.
    [NSApp setActivationPolicy:NSApplicationActivationPolicyRegular];

    POMNavigationDelegateInstance = [[POMNavigationDelegate alloc] init];
    POMScriptHandlerInstance = [[POMScriptHandler alloc] init];
    POMStatusControllerInstance = [[POMStatusController alloc] init];
    POMInstallApplicationMenu();

    POMStatusItem = [[NSStatusBar systemStatusBar]
        statusItemWithLength:NSVariableStatusItemLength];
    NSImage *image = POMStatusImage();
    if (image) {
        POMStatusItem.button.image = image;
        POMStatusItem.button.imagePosition = NSImageOnly;
    } else {
        // Without legible art, a title is still clickable. Better than an item
        // invisible that the user cannot find in the bar.
        POMStatusItem.button.title = @"POM";
    }
    POMStatusItem.button.toolTip = [NSString stringWithFormat:
        POMLocalized(@"status.itemTooltip", @"Open %@"), POMApplicationName];
    POMStatusItem.button.target = POMStatusControllerInstance;
    POMStatusItem.button.action = @selector(togglePopover:);
    [POMStatusItem.button sendActionOn:(NSEventMaskLeftMouseUp | NSEventMaskRightMouseUp)];

    POMOpenWindow();
}

// Clicking the Dock icon brings the window back after it was closed.
- (BOOL)applicationShouldHandleReopen:(NSApplication *)sender hasVisibleWindows:(BOOL)flag {
    (void)sender;
    if (!flag) {
        POMOpenWindow();
    }
    return YES;
}

// Right-clicking the Dock icon offers Quit, so the app can be closed from the Dock
// as well as from the status bar item.
- (NSMenu *)applicationDockMenu:(NSApplication *)sender {
    (void)sender;
    NSMenu *menu = [[NSMenu alloc] initWithTitle:POMApplicationName];
    [menu addItem:[[NSMenuItem alloc]
        initWithTitle:POMLocalized(@"status.quit", @"Quit POM")
               action:@selector(terminate:)
        keyEquivalent:@""]];
    return menu;
}

@end

static void POMInstallApplicationMenu(void) {
    NSString *appName = [[NSBundle mainBundle] objectForInfoDictionaryKey:@"CFBundleDisplayName"];
    if (!appName.length) {
        appName = @"POM";
    }

    NSMenu *mainMenu = [[NSMenu alloc] initWithTitle:appName];
    NSMenuItem *appMenuItem = [[NSMenuItem alloc] initWithTitle:appName
                                                          action:nil
                                                   keyEquivalent:@""];
    NSMenu *appMenu = [[NSMenu alloc] initWithTitle:appName];
    [appMenu addItemWithTitle:POMLocalized(@"status.about", @"About POM")
                        action:@selector(orderFrontStandardAboutPanel:)
                 keyEquivalent:@""];
    [appMenu addItem:[NSMenuItem separatorItem]];
    [appMenu addItem:[[NSMenuItem alloc]
        initWithTitle:POMLocalized(@"status.quit", @"Quit POM")
                action:@selector(terminate:)
         keyEquivalent:@"q"]];
    appMenuItem.submenu = appMenu;
    [mainMenu addItem:appMenuItem];

    // An Edit menu. Without it Cmd+C, Cmd+V and the rest do nothing in the web view:
    // AppKit routes those shortcuts through the menu bar's key equivalents. The
    // actions target the first responder, which is the focused WKWebView.
    NSString *editTitle = POMLocalized(@"status.editMenu", @"Edit");
    NSMenuItem *editMenuItem = [[NSMenuItem alloc] initWithTitle:editTitle
                                                           action:nil
                                                    keyEquivalent:@""];
    NSMenu *editMenu = [[NSMenu alloc] initWithTitle:editTitle];
    [editMenu addItemWithTitle:POMLocalized(@"status.undo", @"Undo")
                        action:@selector(undo:)
                 keyEquivalent:@"z"];
    NSMenuItem *redo = [editMenu addItemWithTitle:POMLocalized(@"status.redo", @"Redo")
                                           action:@selector(redo:)
                                    keyEquivalent:@"z"];
    redo.keyEquivalentModifierMask = NSEventModifierFlagCommand | NSEventModifierFlagShift;
    [editMenu addItem:[NSMenuItem separatorItem]];
    [editMenu addItemWithTitle:POMLocalized(@"status.cut", @"Cut")
                        action:@selector(cut:)
                 keyEquivalent:@"x"];
    [editMenu addItemWithTitle:POMLocalized(@"status.copy", @"Copy")
                        action:@selector(copy:)
                 keyEquivalent:@"c"];
    [editMenu addItemWithTitle:POMLocalized(@"status.paste", @"Paste")
                        action:@selector(paste:)
                 keyEquivalent:@"v"];
    [editMenu addItemWithTitle:POMLocalized(@"status.selectAll", @"Select All")
                        action:@selector(selectAll:)
                 keyEquivalent:@"a"];
    editMenuItem.submenu = editMenu;
    [mainMenu addItem:editMenuItem];

    // A View menu with the browser's Reload: a WebKit window has no reload button,
    // and without one a screen that stopped working has no way back.
    NSString *viewTitle = POMLocalized(@"status.viewMenu", @"View");
    NSMenuItem *viewMenuItem = [[NSMenuItem alloc] initWithTitle:viewTitle
                                                           action:nil
                                                    keyEquivalent:@""];
    NSMenu *viewMenu = [[NSMenu alloc] initWithTitle:viewTitle];
    NSMenuItem *reload = [[NSMenuItem alloc]
        initWithTitle:POMLocalized(@"status.reload", @"Reload")
               action:@selector(reload:)
        keyEquivalent:@"r"];
    reload.target = POMStatusControllerInstance;
    [viewMenu addItem:reload];
    viewMenuItem.submenu = viewMenu;
    [mainMenu addItem:viewMenuItem];

    NSApp.mainMenu = mainMenu;
}

int pom_macos_status_run(const char *url,
                         const char *window_url,
                         const char *icon_path,
                         const char *app_name,
                         double popover_width,
                         double popover_height,
                         double window_width,
                         double window_height) {
    @autoreleasepool {
        // The service thread can finish before AppKit reaches the event
        // loop - an invalid `pom.json` fails in milliseconds. Storing the
        // stop request avoids opening a bar item nobody would see and
        // that should already be closed.
        if (atomic_load(&POMStopRequested)) {
            return 0;
        }
        atomic_store(&POMUserQuitRequested, false);
        POMChatURL = [NSString stringWithUTF8String:url ?: ""];
        POMWindowURL = [NSString stringWithUTF8String:window_url ?: ""];
        POMIconPath = [NSString stringWithUTF8String:icon_path ?: ""];
        POMApplicationName = [NSString stringWithUTF8String:app_name ?: ""];
        if (popover_width > 0 && popover_height > 0) {
            POMPopoverWidth = (CGFloat)popover_width;
            POMPopoverHeight = (CGFloat)popover_height;
        }
        if (window_width > 0 && window_height > 0) {
            POMWindowWidth = (CGFloat)window_width;
            POMWindowHeight = (CGFloat)window_height;
        }

        [NSApplication sharedApplication];
        POMApplicationDelegateInstance = [[POMApplicationDelegate alloc] init];
        NSApp.delegate = POMApplicationDelegateInstance;
        [NSApp run];
    }
    return atomic_load(&POMUserQuitRequested) ? 1 : 0;
}

void pom_macos_status_stop(void) {
    atomic_store(&POMStopRequested, true);
    dispatch_async(dispatch_get_main_queue(), ^{
        if (NSApp) {
            // Stop the AppKit event loop without terminating the process. The
            // Rust service thread owns the real exit status; `terminate:` can
            // end the application directly and discard an async_main error
            // before macos_status.rs joins the thread.
            [NSApp stop:nil];
            NSEvent *wake = [NSEvent otherEventWithType:NSEventTypeApplicationDefined
                                               location:NSZeroPoint
                                          modifierFlags:0
                                              timestamp:0
                                           windowNumber:0
                                                context:nil
                                                subtype:0
                                                  data1:0
                                                  data2:0];
            [NSApp postEvent:wake atStart:YES];
        }
    });
}
