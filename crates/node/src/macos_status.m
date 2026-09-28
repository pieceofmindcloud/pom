#import <AppKit/AppKit.h>
#import <WebKit/WebKit.h>
#import <dispatch/dispatch.h>
#include <stdatomic.h>

static NSString *POMChatURL;
static NSString *POMIconPath;
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

// Medidas do painel e da janela. Quem as escolhe é `macos_status.rs`, que as
// entrega no arranque; aqui elas só têm um valor inicial para o caso de a
// ponte ser chamada com zeros.
static CGFloat POMPopoverWidth = 390.0;
static CGFloat POMPopoverHeight = 844.0;
static CGFloat POMWindowWidth = 1080.0;
static CGFloat POMWindowHeight = 760.0;

static NSString *POMLocalized(NSString *key, NSString *fallback) {
    return [[NSBundle mainBundle] localizedStringForKey:key value:fallback table:nil];
}

static void POMOpenWindow(void);
static void POMShowUnavailable(WKWebView *webView);
static WKWebView *POMMakeWebView(NSRect frame);
static NSImage *POMStatusImage(void);
static void POMInstallApplicationMenu(void);

@interface POMNavigationDelegate : NSObject <WKNavigationDelegate>
@end

@interface POMScriptHandler : NSObject <WKScriptMessageHandler>
@end

@interface POMStatusController : NSObject
- (void)togglePopover:(id)sender;
@end

@interface POMApplicationDelegate : NSObject <NSApplicationDelegate>
@end

/// Altura do painel que cabe no ecrã onde a barra de menus está.
///
/// A medida pedida é a de um telemóvel, 844 pontos. Um MacBook Pro de 13"
/// antigo tem 800 pontos de altura no total, e o painel sairia pela borda de
/// baixo. Aqui ele encolhe para o que sobra abaixo da barra de menus, com uma
/// folga para não encostar no fundo; num ecrã grande a medida pedida passa
/// intacta.
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
        viewController.view = POMPopoverWebView;
        POMPopover.contentViewController = viewController;
    }

    // Recalculado a cada abertura: o portátil pode ter sido ligado a um
    // monitor - ou desligado dele - desde a última vez.
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
        POMWindow.title = POMLocalized(@"status.windowTitle", @"POM chat");
        POMWindow.minSize = NSMakeSize(640, 480);
        POMWindowWebView = POMMakeWebView(NSMakeRect(0, 0, POMWindowWidth, POMWindowHeight));
        POMWindow.contentView = POMWindowWebView;
        [POMWindow center];
    }

    [POMWindow makeKeyAndOrderFront:nil];
    [NSApp activateIgnoringOtherApps:YES];
}

static void POMRetryGateway(WKWebView *webView) {
    NSURL *url = [NSURL URLWithString:POMChatURL];
    if (!url) {
        POMShowUnavailable(webView);
        return;
    }
    [webView loadRequest:[NSURLRequest requestWithURL:url]];
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

static void POMShowUnavailable(WKWebView *webView) {
    [webView loadHTMLString:POMUnavailableHTML() baseURL:[NSURL URLWithString:POMChatURL]];
}

/// Monta a imagem do item de barra com as três resoluções do template.
///
/// `POMIconPath` aponta para a arte 1x; as companheiras `@2x` e `@3x` ficam ao
/// lado dela. Cada representação entra com `size` forçado a 18 x 18 **pontos**,
/// e é isso que faz o AppKit tratá-las como a mesma imagem em escalas
/// diferentes em vez de três imagens de tamanhos diferentes - sem isso, uma
/// arte de 54 px seria desenhada como um glifo de 54 pontos.
///
/// A imagem é marcada como template: o AppKit descarta a cor e pinta o alfa
/// com a cor da barra, o que resolve o tema claro e o escuro de uma só vez,
/// inclusive quando o utilizador troca de tema com o app já aberto.
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

    WKWebView *webView = [[WKWebView alloc] initWithFrame:frame configuration:configuration];
    webView.navigationDelegate = POMNavigationDelegateInstance;
    webView.autoresizingMask = NSViewWidthSizable | NSViewHeightSizable;
    POMRetryGateway(webView);
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
    } else if ([message.name isEqualToString:@"pomRetryGateway"] &&
               [message.webView isKindOfClass:[WKWebView class]]) {
        POMRetryGateway((WKWebView *)message.webView);
    }
}

@end

@implementation POMStatusController

- (void)togglePopover:(id)sender {
    (void)sender;
    POMTogglePopover();
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
    [NSApp setActivationPolicy:NSApplicationActivationPolicyAccessory];

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
        // Sem arte legível, um título continua clicável. Melhor do que um item
        // invisível que o utilizador não consegue encontrar na barra.
        POMStatusItem.button.title = @"POM";
    }
    POMStatusItem.button.toolTip = POMLocalized(@"status.itemTooltip", @"Open POM chat");
    POMStatusItem.button.target = POMStatusControllerInstance;
    POMStatusItem.button.action = @selector(togglePopover:);
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
    NSApp.mainMenu = mainMenu;
}

int pom_macos_status_run(const char *url,
                         const char *icon_path,
                         double popover_width,
                         double popover_height,
                         double window_width,
                         double window_height) {
    @autoreleasepool {
        // A thread do serviço pode terminar antes de o AppKit chegar ao laço
        // de eventos - um `pom.json` inválido falha em milissegundos. Guardar o
        // pedido de paragem evita abrir um item de barra que ninguém veria e
        // que já devia estar fechado.
        if (atomic_load(&POMStopRequested)) {
            return 0;
        }
        atomic_store(&POMUserQuitRequested, false);
        POMChatURL = [NSString stringWithUTF8String:url ?: ""];
        POMIconPath = [NSString stringWithUTF8String:icon_path ?: ""];
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
