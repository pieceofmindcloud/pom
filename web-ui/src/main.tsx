import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { BrowserRouter } from "react-router-dom";
import App from "./App";
import "./index.css";
import { ThemeProvider } from "./lib/theme";
import { ToastProvider } from "./lib/toast";

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
      <ThemeProvider>
        <ToastProvider>
          <BrowserRouter>
            <App />
          </BrowserRouter>
        </ToastProvider>
      </ThemeProvider>
    </QueryClientProvider>
  </StrictMode>,
);
