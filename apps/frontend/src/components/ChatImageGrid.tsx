// The images a Chat answer generated: a grid of thumbnails read from this
// node by hash, one opened full size with Download.
import { Download } from "lucide-react";
import { useEffect, useState } from "react";
import { useI18n } from "../i18n/provider";
import { loadChatImage } from "../lib/chatImage";
import type { ChatImage } from "../lib/types";
import { Modal } from "./Confirm";
import { Button } from "./ui";

function useImageUrl(hash: string): { url: string | null; missing: boolean } {
  const [state, setState] = useState<{ url: string | null; missing: boolean }>({ url: null, missing: false });
  useEffect(() => {
    let alive = true;
    let made: string | null = null;
    loadChatImage(hash)
      .then((url) => {
        made = url;
        if (alive) setState({ url, missing: url == null });
      })
      .catch(() => alive && setState({ url: null, missing: true }));
    return () => {
      alive = false;
      if (made) URL.revokeObjectURL?.(made);
    };
  }, [hash]);
  return state;
}

function Thumbnail({ image, onOpen }: { image: ChatImage; onOpen: (url: string) => void }) {
  const { t } = useI18n();
  const { url, missing } = useImageUrl(image.hash);
  if (missing) {
    return <div className="flex aspect-square items-center justify-center rounded-lg border border-line bg-raised p-3 text-center text-[0.72rem] text-muted">{t("chatImage.missing")}</div>;
  }
  if (!url) return <div className="aspect-square animate-pulse rounded-lg bg-raised" />;
  return (
    <button type="button" onClick={() => onOpen(url)} className="overflow-hidden rounded-lg border border-line">
      <img src={url} alt={t("chatImage.alt", { width: image.width, height: image.height })} className="h-full w-full object-cover" />
    </button>
  );
}

export function ChatImageGrid({ images }: { images: ChatImage[] }) {
  const { t } = useI18n();
  const [open, setOpen] = useState<{ url: string; image: ChatImage } | null>(null);
  return (
    <>
      <div className={`grid gap-2 ${images.length > 1 ? "grid-cols-2" : "grid-cols-1"} max-w-xl`} data-testid="chat-images">
        {images.map((image) => <Thumbnail key={image.hash} image={image} onOpen={(url) => setOpen({ url, image })} />)}
      </div>
      <Modal
        open={open != null}
        title={t("chatImage.title")}
        size="xl"
        onClose={() => setOpen(null)}
        footer={open ? (
          <a href={open.url} download={`${open.image.hash}.${open.image.ext}`}>
            <Button variant="outline"><Download className="h-3.5 w-3.5" />{t("chatImage.download")}</Button>
          </a>
        ) : null}
      >
        {open && <img src={open.url} alt={t("chatImage.alt", { width: open.image.width, height: open.image.height })} className="mx-auto max-h-[75vh]" />}
      </Modal>
    </>
  );
}
