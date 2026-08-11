import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { BrowserRouter } from "react-router-dom";
import App from "./App";
import "./index.css";
import { ThemeProvider } from "./lib/theme";
import { ToastProvider } from "./lib/toast";
import { LocaleProvider } from "./i18n/provider";

const queryClient = new QueryClient({
  defaultOptions: {
    queries: {
      // A node that is mid-restart answers with a connection error; retrying
      // once absorbs that without pretending the cluster is down.
      retry: 1,
      refetchOnWindowFocus: true,
      staleTime: 1000,
    },
  },
});

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <QueryClientProvider client={queryClient}>
      <LocaleProvider>
        <ThemeProvider>
          <ToastProvider>
          {/* Opt in to the v7 behaviour now: it is what this app already
              assumes, and it keeps the dev console free of deprecation noise
              so a real warning stands out. */}
          <BrowserRouter
            future={{ v7_startTransition: true, v7_relativeSplatPath: true }}
          >
            <App />
          </BrowserRouter>
          </ToastProvider>
        </ThemeProvider>
      </LocaleProvider>
    </QueryClientProvider>
  </StrictMode>,
);
