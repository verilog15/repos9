require "rails_helper"

RSpec.describe FollowupExportCsvService do
  subject { described_class.new(casa_case.casa_org) }

  let!(:casa_case) { create(:casa_case) }
  let!(:creator) { create(:user, display_name: "Craig") }
  let!(:alice) { create(:volunteer, display_name: "Alice", casa_org: casa_case.casa_org) }
  let!(:bob) { create(:volunteer, display_name: "Bob", casa_org: casa_case.casa_org) }
  let!(:case_contact) { create(:case_contact, casa_case: casa_case) }
  let!(:followup) { create(:followup, creator: creator, case_contact: case_contact, note: "hello, this is the thing, ") }

  before do
    create(:case_assignment, casa_case: casa_case, volunteer: alice)
    create(:case_assignment, casa_case: casa_case, volunteer: bob)
  end

  describe "#perform" do
    it "Exports case contact followup data" do
      results = subject.perform.split("\n")
      expect(results.count).to eq(2)
      expect(results[0].split(",")).to eq(["Case Number", "Volunteer Name(s)", "Note Creator Name", "Note"])
      expect(results[1]).to eq %(#{case_contact.casa_case.case_number},Alice and Bob,Craig,"#{followup.note}")
    end
  end
end
