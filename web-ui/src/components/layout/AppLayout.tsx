import React from 'react';

interface LayoutProps {
  sidebar: React.ReactNode;
  children: React.ReactNode;
}

export function AppLayout({ sidebar, children }: LayoutProps) {
  return (
    <div className="flex h-screen w-screen overflow-hidden bg-background">
      {/* Sidebar */}
      <aside className="w-64 flex-shrink-0 border-r bg-muted/20 hidden md:flex flex-col">
        {sidebar}
      </aside>

      {/* Main Content */}
      <main className="flex-1 flex flex-col min-w-0">
        {children}
      </main>
    </div>
  );
}
