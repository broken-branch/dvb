import * as React from 'react';
import { cn } from 'cn';

import { useIsMobile } from '@/hooks/use-mobile';
import {
  Sheet,
  SheetContent,
  SheetDescription,
  SheetHeader,
  SheetTitle,
} from '@/components/ui/sheet';

const SIDEBAR_WIDTH = '232px';
const SIDEBAR_WIDTH_MOBILE = '18rem';
const SIDEBAR_WIDTH_ICON = '76px';

type SidebarContextProps = {
  state: 'expanded' | 'collapsed';
  open: boolean;
  setOpen: (open: boolean) => void;
  openMobile: boolean;
  setOpenMobile: (open: boolean) => void;
  isMobile: boolean;
  toggleSidebar: () => void;
};

const SidebarContext = React.createContext<SidebarContextProps | null>(null);

function useSidebar() {
  const context = React.useContext(SidebarContext);
  if (!context) {
    throw new Error('useSidebar must be used within a SidebarProvider.');
  }

  return context;
}

function SidebarProvider({
  defaultOpen = true,
  className,
  style,
  children,
  ...props
}: React.ComponentProps<'div'> & {
  defaultOpen?: boolean;
}) {
  const isMobile = useIsMobile();
  const [openMobile, setOpenMobile] = React.useState(false);

  const [open, setOpen] = React.useState(defaultOpen);

  const toggleSidebar = React.useCallback(() => {
    return isMobile ? setOpenMobile((open) => !open) : setOpen((open) => !open);
  }, [isMobile, setOpen, setOpenMobile]);

  // In-app shortcuts are owned by App; Omarchy owns Super.

  const state = open ? 'expanded' : 'collapsed';

  const contextValue = React.useMemo<SidebarContextProps>(
    () => ({
      state,
      open,
      setOpen,
      isMobile,
      openMobile,
      setOpenMobile,
      toggleSidebar,
    }),
    [state, open, setOpen, isMobile, openMobile, setOpenMobile, toggleSidebar],
  );

  return (
    <SidebarContext.Provider value={contextValue}>
      <div
        data-slot="sidebar-wrapper"
        style={
          {
            '--sidebar-width': SIDEBAR_WIDTH,
            '--sidebar-width-icon': SIDEBAR_WIDTH_ICON,
            ...style,
          } as React.CSSProperties
        }
        className={cn('group/sidebar-wrapper flex min-h-svh w-full', className)}
        {...props}
      >
        {children}
      </div>
    </SidebarContext.Provider>
  );
}

function Sidebar({ className, children, dir, ...props }: React.ComponentProps<'div'>) {
  const { isMobile, state, openMobile, setOpenMobile } = useSidebar();

  if (isMobile) {
    return (
      <Sheet open={openMobile} onOpenChange={setOpenMobile} {...props}>
        <SheetContent
          dir={dir}
          data-sidebar="sidebar"
          data-slot="sidebar"
          data-mobile="true"
          className="w-(--sidebar-width) bg-sidebar p-0 text-sidebar-foreground [&>button]:hidden"
          style={
            {
              '--sidebar-width': SIDEBAR_WIDTH_MOBILE,
            } as React.CSSProperties
          }
          side="left"
        >
          <SheetHeader className="mobile-sheet-header">
            <SheetTitle>Navigation</SheetTitle>
            <SheetDescription className="sr-only">Displays the mobile sidebar.</SheetDescription>
          </SheetHeader>
          <div className="flex min-h-0 flex-1 w-full flex-col">{children}</div>
        </SheetContent>
      </Sheet>
    );
  }

  return (
    <div
      className="group peer hidden text-sidebar-foreground md:block"
      data-state={state}
      data-collapsible={state === 'collapsed' ? 'icon' : ''}
      data-variant="sidebar"
      data-side="left"
      data-slot="sidebar"
    >
      {/* This is what handles the sidebar gap on desktop */}
      <div
        data-slot="sidebar-gap"
        className="relative w-(--sidebar-width) bg-transparent transition-[width] duration-200 ease-linear group-data-[collapsible=icon]:w-(--sidebar-width-icon)"
      />
      <div
        data-slot="sidebar-container"
        data-side="left"
        className={cn(
          'fixed inset-y-0 left-0 z-10 hidden h-svh w-(--sidebar-width) transition-[left,width] duration-200 ease-linear md:flex group-data-[collapsible=icon]:w-(--sidebar-width-icon) border-r',
          className,
        )}
        {...props}
      >
        <div
          data-sidebar="sidebar"
          data-slot="sidebar-inner"
          className="flex size-full flex-col bg-sidebar"
        >
          {children}
        </div>
      </div>
    </div>
  );
}

function SidebarHeader({ className, ...props }: React.ComponentProps<'div'>) {
  return (
    <div
      data-slot="sidebar-header"
      data-sidebar="header"
      className={cn('flex flex-col gap-2 p-2', className)}
      {...props}
    />
  );
}

function SidebarFooter({ className, ...props }: React.ComponentProps<'div'>) {
  return (
    <div
      data-slot="sidebar-footer"
      data-sidebar="footer"
      className={cn('flex flex-col gap-2 p-2', className)}
      {...props}
    />
  );
}

function SidebarContent({ className, ...props }: React.ComponentProps<'div'>) {
  return (
    <div
      data-slot="sidebar-content"
      data-sidebar="content"
      className={cn(
        'no-scrollbar flex min-h-0 flex-1 flex-col gap-0 overflow-auto group-data-[collapsible=icon]:overflow-hidden',
        className,
      )}
      {...props}
    />
  );
}

export { Sidebar, SidebarContent, SidebarFooter, SidebarHeader, SidebarProvider, useSidebar };
