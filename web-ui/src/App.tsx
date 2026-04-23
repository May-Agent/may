import { BrowserRouter, Route, Routes } from 'react-router-dom';
import { AppShell } from '@/components/shell/AppShell';
import { HomePage } from '@/routes/HomePage';
import { SessionPage } from '@/routes/SessionPage';
import { SettingsPage } from '@/routes/SettingsPage';
import { ProfileRedirect } from '@/routes/ProfileRedirect';

export function App() {
  return (
    <BrowserRouter>
      <Routes>
        <Route element={<AppShell />}>
          <Route index element={<ProfileRedirect />} />
          <Route path="/p/:profileId" element={<HomePage />} />
          <Route path="/p/:profileId/s/:sessionId" element={<SessionPage />} />
          <Route path="/settings" element={<SettingsPage />} />
          <Route path="*" element={<ProfileRedirect />} />
        </Route>
      </Routes>
    </BrowserRouter>
  );
}
