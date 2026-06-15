import type { Metadata } from "next";
import { BookOpen } from "lucide-react";
import { Card, CardContent } from "@/components/ui/card";
import { Badge } from "@/components/ui/badge";
import { PageHeader, SectionTitle } from "@/components/common/atoms";
import { Explainer } from "@/components/common/explain";
import { GLOSSARY, GLOSSARY_GROUPS } from "@/lib/glossary";

export const metadata: Metadata = { title: "Glossary" };

export default function GlossaryPage() {
  return (
    <div className="space-y-8">
      <PageHeader
        title="Glossary"
        description="Plain-language explanations of every term used in this console — what it means and why it matters."
        icon={<BookOpen />}
      >
        <Badge variant="muted">{GLOSSARY.length} terms</Badge>
      </PageHeader>

      <Explainer
        what="This whole project lets ordinary machines share their compute to run database queries for each other, with no central server you have to trust."
        impact="The terms below are the building blocks that make that safe and fast: how machines find each other, agree on a correct answer, and (optionally) get paid — each one notes how it affects you."
      />

      {GLOSSARY_GROUPS.map((group) => {
        const items = GLOSSARY.filter((t) => t.group === group);
        return (
          <div key={group}>
            <SectionTitle hint={`${items.length} terms`}>{group}</SectionTitle>
            <div className="grid gap-4 md:grid-cols-2 xl:grid-cols-3">
              {items.map((t) => (
                <Card key={t.term}>
                  <CardContent className="space-y-2 py-4">
                    <div className="text-sm font-semibold">{t.term}</div>
                    <p className="text-muted-foreground text-sm leading-relaxed">{t.what}</p>
                    <p className="text-xs leading-relaxed">
                      <span className="text-primary font-medium">Impact: </span>
                      <span className="text-muted-foreground">{t.impact}</span>
                    </p>
                  </CardContent>
                </Card>
              ))}
            </div>
          </div>
        );
      })}
    </div>
  );
}
